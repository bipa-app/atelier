use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesRef, BytesStart, BytesText, Event};
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::{NsReader, XmlVersion};
use thiserror::Error;
use zip::ZipArchive;
use zip::result::ZipError;

/// The canonical document-body part, used when the package carries no
/// relationships to resolve through.
const DOCUMENT_PART: &str = "word/document.xml";

/// The canonical styles part (optional).
const STYLES_PART: &str = "word/styles.xml";

/// The canonical numbering part (optional).
const NUMBERING_PART: &str = "word/numbering.xml";

/// The package-level relationships part that names the main document.
const PACKAGE_RELS: &str = "_rels/.rels";

/// The OPC relationships namespace.
const RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// The exact relationship types that name each part, transitional and ISO
/// strict. Matching exact URIs — never suffixes — keeps a custom
/// relationship type from spoofing the real one.
const OFFICE_DOCUMENT_TYPES: [&str; 2] = [
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument",
    "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument",
];
const STYLES_TYPES: [&str; 2] = [
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles",
    "http://purl.oclc.org/ooxml/officeDocument/relationships/styles",
];
const NUMBERING_TYPES: [&str; 2] = [
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering",
    "http://purl.oclc.org/ooxml/officeDocument/relationships/numbering",
];

/// The most a compressed part may inflate to. A well-formed Word body runs
/// a few megabytes; anything past this is a decompression bomb, not a
/// document — the outer file cap alone cannot bound it, deflate expands
/// three orders of magnitude.
const PART_SIZE_MAX: u64 = 64 * 1024 * 1024;

/// The namespaces `WordprocessingML` elements live in: the transitional one
/// Word writes, and the ISO strict variant.
const WORDPROCESSINGML: [&str; 2] = [
    "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    "http://purl.oclc.org/ooxml/wordprocessingml/main",
];

/// The markup-compatibility namespace: its `Choice` branches carry content
/// for consumers that support some extension, its `Fallback` the
/// equivalent for those that do not.
const MARKUP_COMPATIBILITY: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

/// Markdown headings stop at six `#`s; deeper Word headings clamp to it.
const HEADING_LEVEL_MAX: usize = 6;

/// Word's list levels run 0..=8 (`w:ilvl`).
const LIST_LEVEL_MAX: usize = 8;

/// `w:outlineLvl` runs 0..=8 for outline levels; 9 means body text.
const OUTLINE_BODY_TEXT: usize = 9;

/// The `w:numId` value that removes numbering a style would otherwise
/// apply, rather than referencing a definition.
const NUMBERING_REMOVED: &str = "0";

/// Nested list items indent three spaces per level: enough to sit inside
/// the content column of both `- ` and `1. ` parent markers, so `CommonMark`
/// keeps the hierarchy.
const LIST_INDENT: &str = "   ";

/// Containers whose subtrees the projection skips. The `*Change` records
/// carry pre-revision properties and `del`/`moveFrom` wrap content a
/// pending revision removes — the accepted body excludes both. Text-box
/// content sits outside the main body flow and is not projected in v1.
pub(crate) const SKIPPED_SUBTREES: [&str; 10] = [
    "pPrChange",
    "rPrChange",
    "tblPrChange",
    "trPrChange",
    "tcPrChange",
    "sectPrChange",
    "numberingChange",
    "del",
    "moveFrom",
    "txbxContent",
];

#[derive(Debug, Error)]
pub(crate) enum DocxError {
    #[error("not a zip archive: {0}")]
    Zip(#[from] ZipError),

    #[error("{DOCUMENT_PART} is missing")]
    MissingDocument,

    #[error("read a document part: {0}")]
    Io(#[from] std::io::Error),

    #[error("malformed document xml: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("unexpected document structure: {0}")]
    Structure(String),
}

/// Project the docx at `bytes` to markdown.
///
/// The walk is a single pass over `word/document.xml` in document order —
/// the same bytes always render the same markdown — with the optional
/// styles and numbering parts read first so headings and list markers
/// resolve.
pub(crate) fn markdown(bytes: &[u8]) -> Result<String, DocxError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let (document, document_path) = document_part(&mut archive)?;
    let document_dir = match document_path.rsplit_once('/') {
        Some((dir, _)) => dir.to_owned(),
        None => String::new(),
    };
    let document_name = match document_path.rsplit_once('/') {
        Some((_, name)) => name,
        None => document_path.as_str(),
    };
    let document_rels = part(
        &mut archive,
        &if document_dir.is_empty() {
            format!("_rels/{document_name}.rels")
        } else {
            format!("{document_dir}/_rels/{document_name}.rels")
        },
    )?;
    let (styles_path, numbering_path) = match &document_rels {
        Some(xml) => (
            relationship_target(xml, &document_dir, &STYLES_TYPES)?,
            relationship_target(xml, &document_dir, &NUMBERING_TYPES)?,
        ),
        None => (
            Some(STYLES_PART.to_owned()),
            Some(NUMBERING_PART.to_owned()),
        ),
    };
    let related = document_rels.is_some();
    let (styles, default_style) =
        match auxiliary_part(&mut archive, styles_path.as_deref(), related, "styles")? {
            Some(xml) => resolved_styles(&xml)?,
            None => (BTreeMap::new(), None),
        };
    let numbering = match auxiliary_part(
        &mut archive,
        numbering_path.as_deref(),
        related,
        "numbering",
    )? {
        Some(xml) => numbering_part(&xml)?,
        None => NumberingPart::default(),
    };
    walk(&document, &styles, default_style.as_deref(), &numbering)
}

/// The main document part and its resolved path, located through the
/// package relationships. OPC locates parts through relationships, not
/// fixed paths, and the relationship graph is authoritative: when a
/// relationships part exists, only what it names counts. The canonical
/// layout applies only to packages whose relationship parts are absent —
/// the minimal producers that omit them always use it.
pub(crate) fn document_part(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
) -> Result<(String, String), DocxError> {
    let document_path = match part(archive, PACKAGE_RELS)? {
        Some(xml) => match relationship_target(&xml, "", &OFFICE_DOCUMENT_TYPES)? {
            Some(target) => target,
            None => {
                return Err(DocxError::Structure(
                    "the package names no officeDocument relationship".to_owned(),
                ));
            }
        },
        None => DOCUMENT_PART.to_owned(),
    };
    match part(archive, &document_path)? {
        Some(document) => Ok((document, document_path)),
        None => Err(DocxError::MissingDocument),
    }
}

/// One auxiliary part by its resolved path. A path named by a
/// relationship must exist — a dangling relationship marks a broken
/// package — while a canonical path is simply optional.
fn auxiliary_part(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    path: Option<&str>,
    related: bool,
    label: &str,
) -> Result<Option<String>, DocxError> {
    let Some(path) = path else {
        return Ok(None);
    };
    match part(archive, path)? {
        Some(xml) => Ok(Some(xml)),
        None if related => Err(DocxError::Structure(format!(
            "the {label} relationship targets a missing part"
        ))),
        None => Ok(None),
    }
}

/// The first internal relationship whose type exactly matches one of
/// `types`, its target resolved against `base`.
fn relationship_target(
    xml: &str,
    base: &str,
    types: &[&str; 2],
) -> Result<Option<String>, DocxError> {
    let mut reader = NsReader::from_str(xml);
    let mut frame = PartFrame::new("relationships", "Relationships");
    let mut found = None;

    loop {
        match reader.read_resolved_event()? {
            (resolve, Event::Start(start)) => {
                frame.start()?;
                let relationships = in_relationships(&checked(resolve)?);
                resolved_attributes(&reader, &start)?;
                if relationships {
                    relationship_element(&reader, &start, types, base, &mut found)?;
                }
            }
            (resolve, Event::Empty(start)) => {
                let relationships = in_relationships(&checked(resolve)?);
                frame.empty(relationships, start.local_name().as_ref())?;
                resolved_attributes(&reader, &start)?;
                if relationships {
                    relationship_element(&reader, &start, types, base, &mut found)?;
                }
            }
            (resolve, Event::End(end)) => {
                let relationships = in_relationships(&checked(resolve)?);
                frame.end(relationships, end.local_name().as_ref())?;
            }
            (_, Event::Text(text)) => {
                let content = character_data(&text)?;
                frame.characters(xml_whitespace_only(&content))?;
            }
            (_, Event::CData(data)) => {
                ensure_xml_chars(&data.xml10_content())?;
                frame.characters(false)?;
            }
            (_, Event::GeneralRef(reference)) => {
                resolve_reference(&reference)?;
                frame.characters(false)?;
            }
            (_, Event::Decl(_) | Event::DocType(_)) => {
                frame.prolog_declaration()?;
            }
            (_, Event::Comment(text)) => {
                ensure_xml_chars(&text.xml10_content())?;
            }
            (_, Event::PI(instruction)) => {
                ensure_xml_chars(&instruction.into_inner())?;
            }
            (_, Event::Eof) => {
                frame.eof()?;
                break;
            }
        }
    }
    Ok(found)
}

fn relationship_element<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    types: &[&str; 2],
    base: &str,
    found: &mut Option<String>,
) -> Result<(), DocxError> {
    if start.local_name().as_ref() != "Relationship" || found.is_some() {
        return Ok(());
    }
    let Some(kind) = unqualified_attribute(reader, start, "Type")? else {
        return Ok(());
    };
    if !types.contains(&kind.as_str()) {
        return Ok(());
    }
    if let Some(mode) = unqualified_attribute(reader, start, "TargetMode")?
        && mode == "External"
    {
        return Ok(());
    }
    if let Some(target) = unqualified_attribute(reader, start, "Target")? {
        *found = Some(resolved_target(base, &target));
    }
    Ok(())
}

/// The value of the unqualified `name` attribute on `start`. OPC
/// relationship attributes carry no namespace — a qualified lookalike
/// (`w:Type`) must not spoof the authoritative graph.
fn unqualified_attribute<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    name: &str,
) -> Result<Option<String>, DocxError> {
    for attribute in start.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        if attribute.key.as_namespace_binding().is_some() {
            continue;
        }
        let (resolve, local) = reader.resolver().resolve_attribute(attribute.key);
        let resolve = checked(resolve)?;
        if local.as_ref() == name && matches!(resolve, ResolveResult::Unbound) {
            return Ok(Some(
                attribute
                    .normalized_value(XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn in_relationships(resolve: &ResolveResult<'_>) -> bool {
    matches!(resolve, ResolveResult::Bound(Namespace(ns)) if *ns == RELATIONSHIPS)
}

/// An OPC target resolved against the part's directory: absolute targets
/// strip their leading slash, relative ones normalize `.` and `..`
/// segments.
fn resolved_target(base: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_owned();
    }
    let mut segments: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            name => segments.push(name),
        }
    }
    segments.join("/")
}

/// One archive part as text, `None` when the archive has no such part.
/// Word writes parts as UTF-8; OOXML also permits UTF-16, which always
/// carries a BOM. Anything else must be valid UTF-8.
fn part(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Option<String>, DocxError> {
    let part = match archive.by_name(name) {
        Ok(part) => part,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(DocxError::Zip(error)),
    };
    let mut bytes = Vec::new();
    part.take(PART_SIZE_MAX + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > PART_SIZE_MAX {
        return Err(DocxError::Structure(format!(
            "{name} inflates past {PART_SIZE_MAX} bytes"
        )));
    }
    decoded(bytes, name).map(Some)
}

fn decoded(bytes: Vec<u8>, name: &str) -> Result<String, DocxError> {
    match bytes.as_slice() {
        [0xff, 0xfe, rest @ ..] => utf16(rest, name, u16::from_le_bytes),
        [0xfe, 0xff, rest @ ..] => utf16(rest, name, u16::from_be_bytes),
        _ => String::from_utf8(bytes)
            .map_err(|_| DocxError::Structure(format!("{name} is not valid UTF-8"))),
    }
}

fn utf16(bytes: &[u8], name: &str, decode: fn([u8; 2]) -> u16) -> Result<String, DocxError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(DocxError::Structure(format!(
            "{name} ends mid UTF-16 code unit"
        )));
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| decode([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units)
        .map_err(|_| DocxError::Structure(format!("{name} is not valid UTF-16")))
}

/// Element balance and root accounting for one part's walk: exactly one
/// root, it must be the expected `WordprocessingML` element, nothing may
/// follow it, and reaching EOF with anything open is truncation.
struct PartFrame {
    label: &'static str,
    root: &'static str,
    depth: usize,
    root_closed: bool,
}

impl PartFrame {
    fn new(label: &'static str, root: &'static str) -> Self {
        Self {
            label,
            root,
            depth: 0,
            root_closed: false,
        }
    }

    fn start(&mut self) -> Result<(), DocxError> {
        if self.depth == 0 && self.root_closed {
            return Err(DocxError::Structure(format!(
                "content after the {} root",
                self.label
            )));
        }
        self.depth += 1;
        Ok(())
    }

    /// A self-closing element: at depth 0 only the part's own root is
    /// valid (its schema children are all optional), and nothing may
    /// follow an already-closed root.
    fn empty(&mut self, wordprocessingml: bool, local_name: &str) -> Result<(), DocxError> {
        if self.depth > 0 {
            return Ok(());
        }
        if self.root_closed {
            return Err(DocxError::Structure(format!(
                "content after the {} root",
                self.label
            )));
        }
        if !(wordprocessingml && local_name == self.root) {
            return Err(DocxError::Structure(format!(
                "the {} root is not the WordprocessingML element",
                self.label
            )));
        }
        self.root_closed = true;
        Ok(())
    }

    /// Character data outside the root — anything but whitespace between
    /// the XML declaration and the root, or after the root closed — is
    /// malformed; inside the root it is the walker's business.
    fn characters(&self, whitespace_only: bool) -> Result<(), DocxError> {
        if self.depth == 0 && !whitespace_only {
            return Err(DocxError::Structure(format!(
                "character data outside the {} root",
                self.label
            )));
        }
        Ok(())
    }

    fn end(&mut self, wordprocessingml: bool, local_name: &str) -> Result<(), DocxError> {
        self.depth = self
            .depth
            .checked_sub(1)
            .ok_or_else(|| DocxError::Structure("end tag without a matching start".to_owned()))?;
        if self.depth == 0 {
            if !(wordprocessingml && local_name == self.root) {
                return Err(DocxError::Structure(format!(
                    "the {} root is not the WordprocessingML element",
                    self.label
                )));
            }
            self.root_closed = true;
        }
        Ok(())
    }

    /// An XML declaration or document-type declaration belongs to the
    /// prolog: anywhere else — inside the root or after it — is malformed.
    fn prolog_declaration(&self) -> Result<(), DocxError> {
        if self.depth > 0 || self.root_closed {
            return Err(DocxError::Structure(format!(
                "declaration outside the {} prolog",
                self.label
            )));
        }
        Ok(())
    }

    fn eof(&self) -> Result<(), DocxError> {
        if self.depth != 0 {
            return Err(DocxError::Structure(format!(
                "{} ended mid-element",
                self.label
            )));
        }
        if !self.root_closed {
            return Err(DocxError::Structure(format!(
                "no closed WordprocessingML {} root",
                self.label
            )));
        }
        Ok(())
    }
}

fn walk(
    xml: &str,
    styles: &BTreeMap<String, ResolvedStyle>,
    default_style: Option<&str>,
    numbering: &NumberingPart,
) -> Result<String, DocxError> {
    let mut document = Document::new(styles, default_style, numbering);
    walk_body(xml, &mut document)?;
    document.ensure_closed()?;
    Ok(document.render())
}

/// What a body walk feeds: `WordprocessingML` element opens and closes
/// plus character content, already restricted to the accepted body —
/// masked subtrees (`mc:Choice` branches, text boxes) never reach a sink,
/// and every well-formedness rule has run.
pub(crate) trait BodySink {
    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError>;
    fn close(&mut self, local_name: &str) -> Result<(), DocxError>;
    fn text(&mut self, content: &str) -> Result<(), DocxError>;
}

/// One pass over `word/document.xml` in document order, feeding `sink`.
pub(crate) fn walk_body<S: BodySink>(xml: &str, sink: &mut S) -> Result<(), DocxError> {
    let mut reader = NsReader::from_str(xml);
    let mut frame = PartFrame::new("document", "document");
    let mut mask = Mask::document();

    loop {
        match reader.read_resolved_event()? {
            (resolve, Event::Start(start)) => {
                frame.start()?;
                let resolve = checked(resolve)?;
                let wordprocessingml = in_wordprocessingml(&resolve);
                mask.open(&resolve, start.local_name().as_ref(), frame.depth);
                resolved_attributes(&reader, &start)?;
                if wordprocessingml && !mask.active() {
                    sink.open(&reader, &start)?;
                }
            }
            (resolve, Event::Empty(start)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.empty(wordprocessingml, start.local_name().as_ref())?;
                resolved_attributes(&reader, &start)?;
                if wordprocessingml && !mask.active() {
                    sink.open(&reader, &start)?;
                    sink.close(start.local_name().as_ref())?;
                }
            }
            (resolve, Event::End(end)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.end(wordprocessingml, end.local_name().as_ref())?;
                if mask.ends(frame.depth) {
                    continue;
                }
                if wordprocessingml {
                    sink.close(end.local_name().as_ref())?;
                }
            }
            (_, Event::Text(text)) => {
                let content = character_data(&text)?;
                frame.characters(xml_whitespace_only(&content))?;
                if !mask.active() {
                    sink.text(&normalized_eol(&content))?;
                }
            }
            (_, Event::CData(data)) => {
                let content = data.xml10_content();
                ensure_xml_chars(&content)?;
                frame.characters(false)?;
                if !mask.active() {
                    sink.text(&normalized_eol(&content))?;
                }
            }
            (_, Event::GeneralRef(reference)) => {
                frame.characters(false)?;
                let content = resolve_reference(&reference)?;
                if !mask.active() {
                    sink.text(&content)?;
                }
            }
            (_, Event::Decl(_) | Event::DocType(_)) => frame.prolog_declaration()?,
            (_, Event::Comment(text)) => {
                ensure_xml_chars(&text.xml10_content())?;
            }
            (_, Event::PI(instruction)) => {
                ensure_xml_chars(&instruction.into_inner())?;
            }
            (_, Event::Eof) => {
                frame.eof()?;
                break;
            }
        }
    }
    Ok(())
}

/// Every attribute must be well formed, on every element — not only the
/// ones a handler reads: an undeclared prefix or a value outside XML's
/// character set is malformed XML.
fn resolved_attributes<R>(reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError> {
    for attribute in start.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        // Validate the normalized value: a character reference like &#1;
        // expands to a forbidden character the raw source hides.
        let value = attribute.normalized_value(XmlVersion::Implicit1_0)?;
        ensure_xml_chars(&value)?;
        if attribute.key.as_namespace_binding().is_some() {
            continue;
        }
        let (resolve, _) = reader.resolver().resolve_attribute(attribute.key);
        checked(resolve)?;
    }
    Ok(())
}

/// XML's ignorable whitespace: space, tab, CR, LF. Unicode spaces such as
/// NBSP are character data and never ignorable.
fn xml_whitespace_only(text: &str) -> bool {
    text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

/// XML end-of-line normalization (XML 1.0 §2.11): CRLF and bare CR read
/// as LF, so equivalent encodings project identically.
fn normalized_eol(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Character data must stay inside XML 1.0's character set; NULs and
/// other forbidden code points mark a malformed document.
fn ensure_xml_chars(text: &str) -> Result<(), DocxError> {
    match text.chars().find(|c| {
        !matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
    }) {
        Some(forbidden) => Err(DocxError::Structure(format!(
            "character U+{:04X} is outside the XML character set",
            forbidden as u32
        ))),
        None => Ok(()),
    }
}

/// One text event's character data under XML 1.0 content rules, checked
/// against the XML character set. End-of-line normalization stays with
/// the body walk — only text a sink consumes needs it.
fn character_data<'a>(text: &BytesText<'a>) -> Result<Cow<'a, str>, DocxError> {
    let content = text.xml10_content();
    ensure_xml_chars(&content)?;
    Ok(content)
}

fn in_wordprocessingml(resolve: &ResolveResult<'_>) -> bool {
    matches!(resolve, ResolveResult::Bound(Namespace(ns)) if WORDPROCESSINGML.contains(ns))
}

fn in_markup_compatibility(resolve: &ResolveResult<'_>) -> bool {
    matches!(resolve, ResolveResult::Bound(Namespace(ns)) if *ns == MARKUP_COMPATIBILITY)
}

/// A resolution that names an undeclared prefix marks malformed XML; a
/// partial walk over it would project silently wrong markdown.
fn checked(resolve: ResolveResult<'_>) -> Result<ResolveResult<'_>, DocxError> {
    if let ResolveResult::Unknown(prefix) = &resolve {
        return Err(DocxError::Structure(format!(
            "undeclared namespace prefix {prefix}"
        )));
    }
    Ok(resolve)
}

/// The masked subtree open right now, when one is: the depth its
/// container opened at. `mc:Choice` branches mask in every walk — this
/// projector supports no extensions, so only the `Fallback` branch
/// contributes — and text-box containers in ANY namespace (strict
/// documents wrap them as `wne:txbxContent`) mask only the document
/// body, where descending into their paragraphs would corrupt the walk.
struct Mask {
    /// Whether text-box containers mask, besides `mc:Choice` branches.
    text_boxes: bool,
    at: Option<usize>,
}

impl Mask {
    /// The document-body mask: `mc:Choice` branches and text boxes.
    fn document() -> Self {
        Self {
            text_boxes: true,
            at: None,
        }
    }

    /// The auxiliary-part mask: `mc:Choice` branches only.
    fn auxiliary() -> Self {
        Self {
            text_boxes: false,
            at: None,
        }
    }

    /// Whether events right now sit inside a masked subtree.
    fn active(&self) -> bool {
        self.at.is_some()
    }

    /// Opens the mask when `local_name` begins a masked container at
    /// `depth` and none is open — a mask never nests inside another.
    fn open(&mut self, resolve: &ResolveResult<'_>, local_name: &str, depth: usize) {
        if self.at.is_some() {
            return;
        }
        if in_markup_compatibility(resolve) && local_name == "Choice"
            || self.text_boxes && local_name == "txbxContent"
        {
            self.at = Some(depth);
        }
    }

    /// Whether the element close that popped to `depth` is still masked:
    /// every close inside the mask is, and the container's own close —
    /// `depth` drops below where it opened — releases the mask last.
    fn ends(&mut self, depth: usize) -> bool {
        let Some(at) = self.at else {
            return false;
        };
        if depth < at {
            self.at = None;
        }
        true
    }
}

/// How a numbering level marks its items: numbered, bulleted, or — for
/// `numFmt` `none` — not at all, as Word renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Bullet,
    Ordered,
    Unmarked,
}

/// One resolved numbering level: its marker kind and — for ordered levels
/// — the value the numbering starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LevelDef {
    kind: ListKind,
    start: u32,
    /// `w:lvlRestart`: `None` restarts on any shallower advance (the
    /// default), 0 never restarts, N restarts only when a level at or
    /// above the 1-based level N advances.
    restart: Option<u32>,
}

/// numId → (list level → definition), abstract definitions resolved and
/// per-instance overrides applied.
type Numbering = BTreeMap<String, BTreeMap<usize, LevelDef>>;

/// The numbering part resolved: marker kinds per definition, the numId →
/// abstract definition bindings, and the paragraph styles each abstract
/// definition's levels associate to (`w:lvl > w:pStyle`).
#[derive(Default)]
struct NumberingPart {
    kinds: Numbering,
    abstracts: BTreeMap<String, String>,
    style_levels: BTreeMap<String, BTreeMap<String, usize>>,
}

impl NumberingPart {
    /// The level the abstract definition behind `num_id` associates with
    /// `style` — OOXML scopes `w:lvl > w:pStyle` to its containing
    /// abstract definition, so an association in another definition never
    /// applies.
    fn style_level(&self, num_id: &str, style: &str) -> Option<usize> {
        let abstract_id = self.abstracts.get(num_id)?;
        self.style_levels.get(abstract_id)?.get(style).copied()
    }
}

/// What a paragraph style contributes once its `basedOn` chain resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedStyle {
    /// The heading level the style names (0..=8); body-text styles carry
    /// none.
    outline: Option<usize>,
    /// The (numId, level) the style's `numPr` names, for lists applied
    /// through styles rather than direct paragraph properties.
    numbering: Option<(String, usize)>,
}

/// The body walk's state: finished blocks plus whatever is open right now.
struct Document<'a> {
    styles: &'a BTreeMap<String, ResolvedStyle>,
    /// The paragraph style `w:default="1"` names: applied when a
    /// paragraph carries no `w:pStyle`, as Word applies it.
    default_style: Option<&'a str>,
    numbering: &'a NumberingPart,
    blocks: Vec<String>,
    paragraph: Option<Paragraph>,
    in_properties: bool,
    /// Inside a run's `w:rPr` — never the paragraph mark's `w:pPr/w:rPr`,
    /// whose formatting belongs to the mark, not to any text.
    in_run_properties: bool,
    in_row_properties: bool,
    in_cell_properties: bool,
    in_text: bool,
    /// The content column each open list level's last marker reached,
    /// keyed by numbering instance: children indent to their own
    /// instance's parent column so `CommonMark` keeps the hierarchy under
    /// markers of any width and unrelated lists never leak state.
    marker_columns: BTreeMap<(String, usize), usize>,
    /// Each numbering instance's next ordinal per level: items advance
    /// within their instance and sublevels restart after a parent, as
    /// Word numbers them.
    ordinals: BTreeMap<(String, usize), u32>,
    /// Depth inside a [`SKIPPED_SUBTREES`] subtree; nothing projects while
    /// > 0.
    skipping: usize,
    /// Text of a paragraph whose mark a tracked deletion removed: the
    /// accepted body merges it into the next paragraph.
    carry: Option<String>,
    tables: Vec<Table>,
}

/// The direct emphasis a run's `w:rPr` declares. Styles-applied emphasis
/// is not projected in v1 — only formatting the document names on the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Emphasis {
    bold: bool,
    italic: bool,
    strike: bool,
}

impl Emphasis {
    fn plain(self) -> bool {
        self == Self::default()
    }

    /// Opening markers, outermost first: strike, bold, italic — one
    /// canonical nesting whatever order the properties appeared in.
    fn opening(self) -> String {
        let mut markers = String::new();
        if self.strike {
            markers.push_str("~~");
        }
        if self.bold {
            markers.push_str("**");
        }
        if self.italic {
            markers.push('*');
        }
        markers
    }

    fn closing(self) -> String {
        let mut markers = String::new();
        if self.italic {
            markers.push('*');
        }
        if self.bold {
            markers.push_str("**");
        }
        if self.strike {
            markers.push_str("~~");
        }
        markers
    }
}

#[derive(Default)]
struct Paragraph {
    style: Option<String>,
    outline: Option<usize>,
    listed: bool,
    num_id: Option<String>,
    level: usize,
    /// Assembled paragraph markdown: inline-escaped text plus bare
    /// emphasis markers. Only line-leading structure remains to escape.
    text: String,
    /// The open span: consecutive content under one emphasis. Word splits
    /// visually identical text into arbitrary runs (proofing state does),
    /// so spans merge across runs — equal documents project equally.
    span: String,
    span_emphasis: Emphasis,
    /// What the current run's `w:rPr` declared for its content.
    run: Emphasis,
    /// A tracked deletion of the paragraph mark: accepting it merges this
    /// paragraph's text into the next.
    mark_deleted: bool,
}

impl Paragraph {
    /// Append run content: inline-escaped into the open span, hard breaks
    /// closing it — markers never cross lines. A change of emphasis since
    /// the span opened closes it first.
    fn append(&mut self, content: &str) {
        let mut first = true;
        for segment in content.split('\n') {
            if !first {
                self.break_line();
            }
            first = false;
            if segment.is_empty() {
                continue;
            }
            if self.run != self.span_emphasis {
                self.flush_span();
                self.span_emphasis = self.run;
            }
            self.span.push_str(&escaped_inline(segment));
        }
    }

    fn break_line(&mut self) {
        self.flush_span();
        self.text.push('\n');
    }

    /// Close the open span, wrapping its non-whitespace core in emphasis
    /// markers. Markdown emphasis cannot flank whitespace: a span's edge
    /// whitespace renders outside its markers, and emphasis on whitespace
    /// alone projects as plain whitespace — Word shows a formatted space
    /// as a space, and property-only differences like it are the Rich
    /// rung's to carry, not the text rung's.
    fn flush_span(&mut self) {
        let span = std::mem::take(&mut self.span);
        if span.is_empty() {
            return;
        }
        if self.span_emphasis.plain() {
            self.text.push_str(&span);
            return;
        }
        let kept = span.trim_end();
        let (kept, trail) = span.split_at(kept.len());
        let lead_len = kept.len() - kept.trim_start().len();
        let (lead, core) = kept.split_at(lead_len);
        if core.is_empty() {
            self.text.push_str(&span);
            return;
        }
        self.text.push_str(lead);
        self.text.push_str(&self.span_emphasis.opening());
        self.text.push_str(core);
        self.text.push_str(&self.span_emphasis.closing());
        self.text.push_str(trail);
    }
}

#[derive(Default)]
struct Table {
    rows: Vec<Row>,
}

#[derive(Default)]
struct Row {
    cells: Vec<Cell>,
    deleted: bool,
}

/// One table cell: its paragraphs kept separate so a paragraph boundary
/// and a literal space never project alike, plus its tracked-deletion
/// mark.
#[derive(Default)]
struct Cell {
    paragraphs: Vec<String>,
    deleted: bool,
}

impl<'a> Document<'a> {
    fn new(
        styles: &'a BTreeMap<String, ResolvedStyle>,
        default_style: Option<&'a str>,
        numbering: &'a NumberingPart,
    ) -> Self {
        Self {
            styles,
            default_style,
            numbering,
            blocks: Vec::new(),
            paragraph: None,
            in_properties: false,
            in_run_properties: false,
            in_row_properties: false,
            in_cell_properties: false,
            in_text: false,
            marker_columns: BTreeMap::new(),
            ordinals: BTreeMap::new(),
            skipping: 0,
            carry: None,
            tables: Vec::new(),
        }
    }
}

impl BodySink for Document<'_> {
    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError> {
        if self.skipping > 0 {
            self.skipping += 1;
            return Ok(());
        }
        let name = start.local_name();
        if SKIPPED_SUBTREES.contains(&name.as_ref()) {
            if name.as_ref() == "del" {
                self.deletion_mark();
            }
            self.skipping = 1;
            return Ok(());
        }
        match name.as_ref() {
            "p" => self.paragraph = Some(Paragraph::default()),
            "pPr" => self.in_properties = true,
            "trPr" => self.in_row_properties = true,
            "tcPr" => self.in_cell_properties = true,
            // A tracked cell deletion: the cell leaves the accepted body,
            // like rows marked deleted in their trPr.
            "cellDel" if self.in_cell_properties => {
                if let Some(cell) = self.open_cell() {
                    cell.deleted = true;
                }
            }
            "pStyle" | "outlineLvl" | "numPr" | "numId" | "ilvl" if self.in_properties => {
                self.paragraph_property(reader, start)?;
            }
            "r" => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.run = Emphasis::default();
                }
            }
            "rPr" if !self.in_properties => self.in_run_properties = true,
            "b" | "i" | "strike" if self.in_run_properties => {
                self.run_property(reader, start)?;
            }
            "t" => self.in_text = true,
            "tab" | "sym" | "noBreakHyphen" | "softHyphen" if !self.in_properties => {
                self.run_character(reader, start)?;
            }
            "br" | "cr" => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.break_line();
                }
            }
            "tbl" => self.tables.push(Table::default()),
            "tr" => match self.tables.last_mut() {
                Some(table) => table.rows.push(Row::default()),
                None => {
                    return Err(DocxError::Structure("table row outside a table".to_owned()));
                }
            },
            "tc" => match self.open_row() {
                Some(row) => row.cells.push(Cell::default()),
                None => {
                    return Err(DocxError::Structure(
                        "table cell outside a table row".to_owned(),
                    ));
                }
            },
            _ => {}
        }
        Ok(())
    }

    fn close(&mut self, local_name: &str) -> Result<(), DocxError> {
        if self.skipping > 0 {
            self.skipping -= 1;
            return Ok(());
        }
        match local_name {
            "pPr" => self.in_properties = false,
            "rPr" => self.in_run_properties = false,
            "trPr" => self.in_row_properties = false,
            "tcPr" => self.in_cell_properties = false,
            "t" => self.in_text = false,
            "p" => {
                let mut paragraph = self.paragraph.take().ok_or_else(|| {
                    DocxError::Structure("paragraph end without a paragraph".to_owned())
                })?;
                paragraph.flush_span();
                // Paragraphs inside a deleted row or cell neither consume
                // nor produce merge text: their content — even under a
                // deleted paragraph mark — never reaches the accepted
                // body, and carried text must not leak through them.
                if self.in_deleted_container() {
                    self.finish_paragraph(&paragraph)?;
                } else {
                    if let Some(carried) = self.carry.take() {
                        paragraph.text = format!("{carried}{}", paragraph.text);
                    }
                    if paragraph.mark_deleted && self.skipping == 0 {
                        // The accepted body merges this text into the
                        // next paragraph instead of ending one here.
                        self.carry = Some(paragraph.text);
                    } else {
                        self.finish_paragraph(&paragraph)?;
                    }
                }
            }
            // A document can end on a paragraph whose mark was deleted
            // with nothing following; its carried text still projects.
            "body" => {
                if let Some(carried) = self.carry.take() {
                    self.finish_paragraph(&Paragraph {
                        text: carried,
                        ..Paragraph::default()
                    })?;
                }
            }
            "tbl" => {
                let table = self
                    .tables
                    .pop()
                    .ok_or_else(|| DocxError::Structure("table end without a table".to_owned()))?;
                if let Some(rendered) = render_table(&table) {
                    self.push_block(rendered)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn text(&mut self, content: &str) -> Result<(), DocxError> {
        if self.skipping > 0 || !self.in_text {
            return Ok(());
        }
        match self.paragraph.as_mut() {
            Some(paragraph) => {
                paragraph.append(content);
                Ok(())
            }
            None => Err(DocxError::Structure("text outside a paragraph".to_owned())),
        }
    }
}

impl Document<'_> {
    /// A tracked deletion mark (`w:del`) applied to what it annotates: in
    /// row properties it deletes the row; on a paragraph mark (inside
    /// `w:pPr/w:rPr`) the accepted body merges this paragraph into the
    /// next.
    fn deletion_mark(&mut self) {
        if self.in_row_properties {
            if let Some(row) = self.open_row() {
                row.deleted = true;
            }
        } else if self.in_properties
            && let Some(paragraph) = self.paragraph.as_mut()
        {
            paragraph.mark_deleted = true;
        }
    }

    /// One paragraph property (`w:pPr` child) resolved onto the open
    /// paragraph: the style, outline level, and numbering it names.
    fn paragraph_property<R>(
        &mut self,
        reader: &NsReader<R>,
        start: &BytesStart<'_>,
    ) -> Result<(), DocxError> {
        match start.local_name().as_ref() {
            "pStyle" => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.style = attribute(reader, start, "val")?;
                }
            }
            "outlineLvl" => {
                if let (Some(paragraph), Some(value)) =
                    (self.paragraph.as_mut(), attribute(reader, start, "val")?)
                {
                    paragraph.outline = Some(outline_level(&value)?);
                }
            }
            "numPr" => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.listed = true;
                }
            }
            "numId" => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.num_id = attribute(reader, start, "val")?;
                }
            }
            "ilvl" => {
                if let (Some(paragraph), Some(level)) =
                    (self.paragraph.as_mut(), attribute(reader, start, "val")?)
                {
                    paragraph.level = list_level(&level)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// One run property (`w:rPr` child) resolved onto the open run: the
    /// direct emphasis — bold, italic, strikethrough — the document names
    /// on it.
    fn run_property<R>(
        &mut self,
        reader: &NsReader<R>,
        start: &BytesStart<'_>,
    ) -> Result<(), DocxError> {
        let Some(paragraph) = self.paragraph.as_mut() else {
            return Ok(());
        };
        let applies = on_off(attribute(reader, start, "val")?.as_deref())?;
        match start.local_name().as_ref() {
            "b" => paragraph.run.bold = applies,
            "i" => paragraph.run.italic = applies,
            "strike" => paragraph.run.strike = applies,
            _ => {}
        }
        Ok(())
    }

    /// One visible run character Word encodes as an element rather than
    /// text — a tab, a symbol, a hyphen: erasing them would project
    /// distinct sentences identically.
    fn run_character<R>(
        &mut self,
        reader: &NsReader<R>,
        start: &BytesStart<'_>,
    ) -> Result<(), DocxError> {
        let Some(paragraph) = self.paragraph.as_mut() else {
            return Ok(());
        };
        match start.local_name().as_ref() {
            "tab" => paragraph.append("\t"),
            "sym" => {
                let value = attribute(reader, start, "char")?.ok_or_else(|| {
                    DocxError::Structure("sym without a char attribute".to_owned())
                })?;
                paragraph.append(&sym_char(&value)?.to_string());
            }
            "noBreakHyphen" => paragraph.append("\u{2011}"),
            "softHyphen" => paragraph.append("\u{00ad}"),
            _ => {}
        }
        Ok(())
    }

    /// Route a finished paragraph: into the open table cell when a table is
    /// open, else into the body as its markdown block.
    ///
    /// Every line of paragraph text is markdown-escaped and the structural
    /// prefix — heading marks, then any list marker — is prepended, so
    /// markers are the only unescaped structure and no document text can
    /// impersonate one: distinct documents never project alike.
    fn finish_paragraph(&mut self, paragraph: &Paragraph) -> Result<(), DocxError> {
        // An empty paragraph still projects — as an empty block — so
        // inserting or removing one is a visible edit.
        let text = if paragraph.text.is_empty() {
            String::new()
        } else {
            escaped_markdown(&paragraph.text)
        };
        if self.in_deleted_container() {
            // Content a tracked deletion removes never renders and must
            // not advance shared numbering or indentation state.
            return self.push_into_cell(text);
        }
        let listed = self
            .listing(paragraph)
            .map(|(num_id, level)| (num_id.map(str::to_owned), level));
        if let Some((Some(id), level)) = &listed {
            self.restart_sublevels(id, *level);
        }
        let list_item = match listed {
            Some((Some(id), level)) => {
                let def = self.level_def(Some(&id), level);
                match def.kind {
                    ListKind::Ordered => {
                        let marker = format!("{}.", self.next_ordinal(&id, level, def.start));
                        Some((marker, level, id))
                    }
                    ListKind::Bullet => Some(("-".to_owned(), level, id)),
                    ListKind::Unmarked => None,
                }
            }
            // A `numPr` without a `numId` carries no definition: unmarked.
            Some((None, _)) | None => None,
        };
        let block = match (self.heading(paragraph), list_item) {
            (Some(heading), Some((marker, _, _))) => {
                self.marker_columns.clear();
                format!("{} {marker} {text}", "#".repeat(heading))
            }
            (Some(heading), None) => {
                self.marker_columns.clear();
                format!("{} {text}", "#".repeat(heading))
            }
            (None, Some((marker, level, id))) => {
                let indent = self.list_indent(&id, level);
                self.marker_columns
                    .retain(|(owner, open), _| owner != &id || *open < level);
                self.marker_columns.insert(
                    (id, level),
                    indent.chars().count() + marker.chars().count() + 1,
                );
                format!("{indent}{marker} {text}")
            }
            // An unmarked level and a non-list paragraph render the same
            // way Word shows them: markerless plain text. Either ends any
            // open list, so stale marker columns clear; ordinals persist —
            // Word continues numbering across interleaved paragraphs.
            (None, None) => {
                self.marker_columns.clear();
                text
            }
        };
        if self.tables.is_empty() {
            self.blocks.push(block);
            return Ok(());
        }
        // Cell paragraphs keep their structure too: a bulleted paragraph
        // in a cell must never project like a plain one.
        self.push_into_cell(block)
    }

    /// A parent item advancing at `advancing` — whatever its marker kind
    /// — restarts deeper ordered levels of the same instance, each per
    /// its own `w:lvlRestart` policy.
    fn restart_sublevels(&mut self, id: &str, advancing: usize) {
        let doomed: Vec<(String, usize)> = self
            .ordinals
            .keys()
            .filter(|(owner, deeper)| owner.as_str() == id && *deeper > advancing)
            .filter(|(_, deeper)| {
                match self.level_def(Some(id), *deeper).restart {
                    // Default: any shallower advance restarts.
                    None => true,
                    // 0: this level never restarts.
                    Some(0) => false,
                    // N: only levels at or above the 1-based level N
                    // restart it.
                    Some(threshold) => advancing < threshold as usize,
                }
            })
            .cloned()
            .collect();
        for key in doomed {
            self.ordinals.remove(&key);
        }
    }

    /// The indentation a list item at `level` renders with: its own
    /// numbering instance's parent marker content column, so `CommonMark`
    /// keeps the hierarchy under markers of any width ("10. " needs four
    /// columns) and unrelated instances never leak indentation. A child
    /// with no rendered parent falls back to three spaces per level.
    fn list_indent(&self, id: &str, level: usize) -> String {
        if level == 0 {
            return String::new();
        }
        match level
            .checked_sub(1)
            .and_then(|parent| self.marker_columns.get(&(id.to_owned(), parent)))
        {
            Some(column) => " ".repeat(*column),
            None => LIST_INDENT.repeat(level),
        }
    }

    /// The next ordinal of `id`'s list at `level`, advancing the
    /// instance's counter. Sublevel restarts happen when any parent item
    /// finishes, whatever its marker kind.
    fn next_ordinal(&mut self, id: &str, level: usize, start: u32) -> u32 {
        let counter = self.ordinals.entry((id.to_owned(), level)).or_insert(start);
        let value = *counter;
        *counter = counter.saturating_add(1);
        value
    }

    /// Whether any open table's current row or cell is marked deleted —
    /// content nested under a deleted container, however deep, never
    /// reaches the rendered output.
    fn in_deleted_container(&self) -> bool {
        self.tables.iter().any(|table| {
            table
                .rows
                .last()
                .is_some_and(|row| row.deleted || row.cells.last().is_some_and(|cell| cell.deleted))
        })
    }

    /// The heading level a paragraph renders at: a direct `w:outlineLvl`
    /// overrides the style, a `Heading1..9` style names its level, and any
    /// other style resolves through the styles part's outline levels.
    fn heading(&self, paragraph: &Paragraph) -> Option<usize> {
        if let Some(outline) = paragraph.outline {
            return (outline != OUTLINE_BODY_TEXT).then(|| (outline + 1).min(HEADING_LEVEL_MAX));
        }
        let style = paragraph.style.as_deref().or(self.default_style)?;
        if let Some(level) = heading_level(style) {
            return Some(level);
        }
        self.styles
            .get(style)?
            .outline
            .map(|outline| (outline + 1).min(HEADING_LEVEL_MAX))
    }

    /// The list a paragraph belongs to in the accepted body: its direct
    /// `numPr` when present — where `numId` 0 removes numbering a style
    /// would apply — else the numbering its style carries. For
    /// style-applied lists, a level the numbering part associates with the
    /// style (`w:lvl > w:pStyle`) overrides the style's own `ilvl`.
    fn listing<'p>(&'p self, paragraph: &'p Paragraph) -> Option<(Option<&'p str>, usize)> {
        if paragraph.listed {
            return match paragraph.num_id.as_deref() {
                Some(NUMBERING_REMOVED) => None,
                Some(id) => Some((Some(id), paragraph.level)),
                None => Some((None, paragraph.level)),
            };
        }
        let style = paragraph.style.as_deref().or(self.default_style)?;
        let (num_id, level) = self.styles.get(style)?.numbering.as_ref()?;
        if num_id == NUMBERING_REMOVED {
            return None;
        }
        let level = match self.numbering.style_level(num_id, style) {
            Some(associated) => associated,
            None => *level,
        };
        Some((Some(num_id.as_str()), level))
    }

    /// The level definition a listed paragraph renders with. A definition
    /// the numbering part does not carry — a missing part, an unknown
    /// `numId`, an undefined level — renders unmarked, exactly as Word
    /// shows such paragraphs.
    fn level_def(&self, num_id: Option<&str>, level: usize) -> LevelDef {
        let unmarked = LevelDef {
            kind: ListKind::Unmarked,
            start: 1,
            restart: None,
        };
        let Some(num_id) = num_id else {
            return unmarked;
        };
        match self
            .numbering
            .kinds
            .get(num_id)
            .and_then(|levels| levels.get(&level))
        {
            Some(def) => *def,
            None => unmarked,
        }
    }

    /// The row open right now: the last row of the innermost open table.
    fn open_row(&mut self) -> Option<&mut Row> {
        self.tables
            .last_mut()
            .and_then(|table| table.rows.last_mut())
    }

    /// The cell open right now: the last cell of the open row.
    fn open_cell(&mut self) -> Option<&mut Cell> {
        self.open_row().and_then(|row| row.cells.last_mut())
    }

    /// A finished block lands in the body, or into the open cell when it
    /// closed inside another table.
    fn push_block(&mut self, block: String) -> Result<(), DocxError> {
        if self.tables.is_empty() {
            self.blocks.push(block);
            return Ok(());
        }
        self.push_into_cell(block)
    }

    fn push_into_cell(&mut self, content: String) -> Result<(), DocxError> {
        let cell = self.open_cell().ok_or_else(|| {
            DocxError::Structure("paragraph inside a table but outside a cell".to_owned())
        })?;
        cell.paragraphs.push(content);
        Ok(())
    }

    /// Everything opened must have closed by the end of the part; a
    /// truncated document would otherwise project to silently shortened
    /// markdown.
    fn ensure_closed(&self) -> Result<(), DocxError> {
        if self.paragraph.is_some()
            || self.in_text
            || self.in_properties
            || self.in_run_properties
            || self.in_row_properties
            || self.in_cell_properties
            || self.skipping > 0
            || self.carry.is_some()
            || !self.tables.is_empty()
        {
            return Err(DocxError::Structure(
                "document ended mid-element".to_owned(),
            ));
        }
        Ok(())
    }

    fn render(self) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }
        let mut rendered = self.blocks.join("\n\n");
        rendered.push('\n');
        rendered
    }
}

/// Inline characters that could impersonate emphasis markers — `*`, `_`,
/// `~` — escape wherever they appear, backslashes first so a generated
/// escape never collides with a literal one. Markers are inserted after
/// this, so a bare `*` or `~` in a projection is always a marker and a
/// document that literally says `**x**` never projects like bold `x`.
fn escaped_inline(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '*' => escaped.push_str("\\*"),
            '_' => escaped.push_str("\\_"),
            '~' => escaped.push_str("\\~"),
            _ => escaped.push(c),
        }
    }
    escaped
}

/// An `ST_OnOff` toggle value: an absent `w:val` turns the property on,
/// as Word reads it. Anything outside the schema's lexicon is malformed —
/// guessing would project silently wrong emphasis.
pub(crate) fn on_off(value: Option<&str>) -> Result<bool, DocxError> {
    match value {
        None | Some("1" | "true" | "on") => Ok(true),
        Some("0" | "false" | "off") => Ok(false),
        Some(other) => Err(DocxError::Structure(format!(
            "on/off value {other:?} is outside ST_OnOff"
        ))),
    }
}

/// Assembled paragraph lines whose first characters could impersonate
/// line-leading markdown structure — `# `, `1. `, `- `, `>`, `|`, a fence
/// — escape the marker, so a paragraph that merely *says* "1. Scope"
/// stays distinguishable from a real list item and the two can never
/// project identically. Inline channels (`\`, `*`, `_`, `~`) escaped at
/// span assembly; a line-leading `*` or `~` here is an emphasis marker
/// and must stay one.
fn escaped_markdown(text: &str) -> String {
    let escaped: Vec<String> = text
        .split('\n')
        .map(|line| {
            // An empty line inside a paragraph renders as a lone
            // backslash — text can never produce one (its backslashes
            // escape), so consecutive hard breaks stay distinct from a
            // paragraph boundary's blank line.
            if line.is_empty() {
                "\\".to_owned()
            } else {
                escaped_markdown_line(line)
            }
        })
        .collect();
    escaped.join("\n")
}

fn escaped_markdown_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return line.to_owned();
    };
    let second = chars.next();
    let escapes = match first {
        '#' | '>' | '|' | '`' => true,
        '-' | '+' => second.is_none_or(|c| c == ' ' || c == '\t' || c == first),
        '0'..='9' => {
            let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
            matches!(
                &trimmed.as_bytes()[digits..],
                [b'.' | b')'] | [b'.' | b')', b' ' | b'\t', ..]
            )
        }
        _ => false,
    };
    if !escapes {
        return line.to_owned();
    }
    if first.is_ascii_digit() {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        return format!("{indent}{}\\{}", &trimmed[..digits], &trimmed[digits..]);
    }
    format!("{indent}\\{trimmed}")
}

/// A `w:start`/`w:startOverride` value: the number an ordered level
/// begins at.
fn list_start(value: &str) -> Result<u32, DocxError> {
    value
        .parse()
        .map_err(|_| DocxError::Structure(format!("list start {value:?} is out of range")))
}

/// The character a `w:sym` names (an `ST_ShortHexNumber`). The symbol's
/// font is not carried: two symbols differing only by font project alike.
pub(crate) fn sym_char(value: &str) -> Result<char, DocxError> {
    let code = u32::from_str_radix(value, 16)
        .map_err(|_| DocxError::Structure(format!("sym char {value:?} is not hex")))?;
    char::from_u32(code)
        .ok_or_else(|| DocxError::Structure(format!("sym char {value:?} is not a character")))
}

/// The heading level a paragraph style names, when it is a heading style.
///
/// Word's built-in heading styles are `Heading1`..`Heading9`; any other
/// style value resolves through the styles part instead.
fn heading_level(style: &str) -> Option<usize> {
    // Exactly Heading1..Heading9 are Word's built-ins; any other id —
    // "Heading10", "Heading01" — is a custom style that resolves through
    // the styles part instead.
    let suffix = style.strip_prefix("Heading")?;
    if suffix.len() != 1 {
        return None;
    }
    let level: usize = suffix.parse().ok()?;
    if !(1..=9).contains(&level) {
        return None;
    }
    Some(level.min(HEADING_LEVEL_MAX))
}

/// A `w:outlineLvl` value: 0..=8 name outline levels, 9 names body text;
/// anything else is out of range.
fn outline_level(value: &str) -> Result<usize, DocxError> {
    let level: usize = value
        .parse()
        .map_err(|_| DocxError::Structure(format!("outline level {value:?} is not a number")))?;
    if level > OUTLINE_BODY_TEXT {
        return Err(DocxError::Structure(format!(
            "outline level {level} is out of range"
        )));
    }
    Ok(level)
}

/// A `w:ilvl` value, bounded to Word's 0..=8 — an unbounded level would
/// drive indentation allocation.
fn list_level(value: &str) -> Result<usize, DocxError> {
    let level: usize = value
        .parse()
        .map_err(|_| DocxError::Structure(format!("list level {value:?} is not a number")))?;
    if level > LIST_LEVEL_MAX {
        return Err(DocxError::Structure(format!(
            "list level {level} is out of range"
        )));
    }
    Ok(level)
}

/// One style's raw definition while the styles part is walked. `ilvl`
/// starts at 0 because a `numPr` without one names level 0, as Word does.
struct StyleDef {
    based_on: Option<String>,
    outline: Option<usize>,
    num_id: Option<String>,
    ilvl: usize,
}

/// What an auxiliary-part walk feeds: `WordprocessingML` element opens
/// and closes, already outside `mc:Choice` branches and
/// [`SKIPPED_SUBTREES`] subtrees, with every well-formedness rule run.
/// Auxiliary parts carry no projected text, so unlike [`BodySink`] there
/// is no text hook — character data is validated and dropped.
trait AuxiliarySink {
    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError>;
    fn close(&mut self, local_name: &str);
}

/// Depth inside a [`SKIPPED_SUBTREES`] subtree during an auxiliary-part
/// walk; nothing reaches the sink while inside one. The document walk
/// keeps its own counter in the sink instead, because `w:del` marks
/// state there before its subtree is skipped.
#[derive(Default)]
struct Skip(usize);

impl Skip {
    /// Whether `local_name`'s open is skipped: everything inside an open
    /// window is — deepening it — and a [`SKIPPED_SUBTREES`] container
    /// opens one.
    fn start(&mut self, local_name: &str) -> bool {
        if self.0 > 0 {
            self.0 += 1;
            return true;
        }
        if SKIPPED_SUBTREES.contains(&local_name) {
            self.0 = 1;
            return true;
        }
        false
    }

    /// Whether a self-closing element is skipped: inside an open window
    /// or itself a [`SKIPPED_SUBTREES`] element. Neither moves the depth.
    fn empty(&self, local_name: &str) -> bool {
        self.0 > 0 || SKIPPED_SUBTREES.contains(&local_name)
    }

    /// Whether an element close is skipped, retreating out of the window.
    fn end(&mut self) -> bool {
        if self.0 > 0 {
            self.0 -= 1;
            return true;
        }
        false
    }
}

/// One pass over an auxiliary part (styles or numbering) in document
/// order, feeding `sink` the element opens and closes that survive
/// masking and skipping.
fn walk_auxiliary<S: AuxiliarySink>(
    xml: &str,
    mut frame: PartFrame,
    sink: &mut S,
) -> Result<(), DocxError> {
    let mut reader = NsReader::from_str(xml);
    let mut mask = Mask::auxiliary();
    let mut skip = Skip::default();
    loop {
        match reader.read_resolved_event()? {
            (resolve, Event::Start(start)) => {
                frame.start()?;
                let resolve = checked(resolve)?;
                let wordprocessingml = in_wordprocessingml(&resolve);
                mask.open(&resolve, start.local_name().as_ref(), frame.depth);
                resolved_attributes(&reader, &start)?;
                if !wordprocessingml || mask.active() {
                    continue;
                }
                if skip.start(start.local_name().as_ref()) {
                    continue;
                }
                sink.open(&reader, &start)?;
            }
            (resolve, Event::Empty(start)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.empty(wordprocessingml, start.local_name().as_ref())?;
                resolved_attributes(&reader, &start)?;
                if !wordprocessingml || mask.active() || skip.empty(start.local_name().as_ref()) {
                    continue;
                }
                sink.open(&reader, &start)?;
            }
            (resolve, Event::End(end)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.end(wordprocessingml, end.local_name().as_ref())?;
                if mask.ends(frame.depth) {
                    continue;
                }
                if !wordprocessingml {
                    continue;
                }
                if skip.end() {
                    continue;
                }
                sink.close(end.local_name().as_ref());
            }
            (_, Event::Text(text)) => {
                let content = character_data(&text)?;
                frame.characters(xml_whitespace_only(&content))?;
            }
            (_, Event::CData(data)) => {
                ensure_xml_chars(&data.xml10_content())?;
                frame.characters(false)?;
            }
            (_, Event::GeneralRef(reference)) => {
                resolve_reference(&reference)?;
                frame.characters(false)?;
            }
            (_, Event::Decl(_) | Event::DocType(_)) => frame.prolog_declaration()?,
            (_, Event::Comment(text)) => {
                ensure_xml_chars(&text.xml10_content())?;
            }
            (_, Event::PI(instruction)) => {
                ensure_xml_chars(&instruction.into_inner())?;
            }
            (_, Event::Eof) => {
                frame.eof()?;
                break;
            }
        }
    }
    Ok(())
}

/// What each paragraph style contributes, `basedOn` chains followed:
/// outline levels for headings, and `numPr` numbering for lists applied
/// through styles. Historical `*Change` records inside style definitions
/// are excluded, as in the document walk.
fn resolved_styles(
    xml: &str,
) -> Result<(BTreeMap<String, ResolvedStyle>, Option<String>), DocxError> {
    let mut walk = StylesWalk::default();
    walk_auxiliary(xml, PartFrame::new("styles", "styles"), &mut walk)?;

    let chains = resolved_chains(&walk.styles)?;
    let mut resolved = BTreeMap::new();
    for (id, chain) in chains {
        let outline = chain.outline.filter(|level| *level != OUTLINE_BODY_TEXT);
        if outline.is_some() || chain.numbering.is_some() {
            resolved.insert(
                id,
                ResolvedStyle {
                    outline,
                    numbering: chain.numbering,
                },
            );
        }
    }
    Ok((resolved, walk.default_style))
}

/// What a style's `basedOn` chain accumulates: the nearest definition of
/// each property wins.
#[derive(Clone, Default)]
struct Chain {
    outline: Option<usize>,
    numbering: Option<(String, usize)>,
}

/// Every style's chain resolved once, with path compression, so resolution
/// stays linear in the style count — per-style chain walks would go
/// quadratic on deep `basedOn` chains. A cycle errors.
fn resolved_chains(
    styles: &BTreeMap<String, StyleDef>,
) -> Result<BTreeMap<String, Chain>, DocxError> {
    let mut memo: BTreeMap<String, Chain> = BTreeMap::new();
    for id in styles.keys() {
        if memo.contains_key(id) {
            continue;
        }
        // Walk up the chain to something already resolved or terminal,
        // remembering the path.
        let mut path: Vec<&str> = Vec::new();
        let mut on_path: BTreeSet<&str> = BTreeSet::new();
        let mut current: &str = id;
        let mut base = Chain::default();
        loop {
            if let Some(done) = memo.get(current) {
                base = done.clone();
                break;
            }
            if on_path.contains(current) {
                return Err(DocxError::Structure(format!(
                    "style {current:?} sits on a basedOn cycle"
                )));
            }
            let Some(def) = styles.get(current) else {
                break;
            };
            path.push(current);
            on_path.insert(current);
            match def.based_on.as_deref() {
                Some(next) => current = next,
                None => break,
            }
        }
        // Fold back down: each style's own properties override what it
        // inherits, and every style on the path memoizes.
        for step in path.into_iter().rev() {
            let mut chain = base.clone();
            if let Some(def) = styles.get(step) {
                if let Some(outline) = def.outline {
                    chain.outline = Some(outline);
                }
                if let Some(num_id) = &def.num_id {
                    chain.numbering = Some((num_id.clone(), def.ilvl));
                }
            }
            memo.insert(step.to_owned(), chain.clone());
            base = chain;
        }
    }
    Ok(memo)
}

/// The styles walk's state while the part streams by: collected
/// definitions, the definition open right now, and the default paragraph
/// style.
#[derive(Default)]
struct StylesWalk {
    styles: BTreeMap<String, StyleDef>,
    current: Option<(String, StyleDef)>,
    default_style: Option<String>,
}

impl AuxiliarySink for StylesWalk {
    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError> {
        style_element(reader, start, self)
    }

    /// A closing `w:style` completes the open definition.
    fn close(&mut self, local_name: &str) {
        if local_name == "style"
            && let Some((id, def)) = self.current.take()
        {
            self.styles.insert(id, def);
        }
    }
}

/// One element inside a style definition: the properties a style
/// contributes to paragraphs that name it.
fn style_element<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    walk: &mut StylesWalk,
) -> Result<(), DocxError> {
    match start.local_name().as_ref() {
        "style" => {
            if let Some(id) = attribute(reader, start, "styleId")? {
                // The default paragraph style applies to paragraphs that
                // name no style at all, as Word applies it.
                let kind = attribute(reader, start, "type")?;
                let default = attribute(reader, start, "default")?;
                if walk.default_style.is_none()
                    && kind.as_deref() == Some("paragraph")
                    && matches!(default.as_deref(), Some("1" | "true" | "on"))
                {
                    walk.default_style = Some(id.clone());
                }
                walk.current = Some((
                    id,
                    StyleDef {
                        based_on: None,
                        outline: None,
                        num_id: None,
                        ilvl: 0,
                    },
                ));
            }
        }
        "basedOn" => {
            if let Some((_, def)) = walk.current.as_mut() {
                def.based_on = attribute(reader, start, "val")?;
            }
        }
        "outlineLvl" => {
            if let (Some((_, def)), Some(value)) =
                (walk.current.as_mut(), attribute(reader, start, "val")?)
            {
                def.outline = Some(outline_level(&value)?);
            }
        }
        "numId" => {
            if let Some((_, def)) = walk.current.as_mut() {
                def.num_id = attribute(reader, start, "val")?;
            }
        }
        "ilvl" => {
            if let (Some((_, def)), Some(value)) =
                (walk.current.as_mut(), attribute(reader, start, "val")?)
            {
                def.ilvl = list_level(&value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// One numbering level while its part streams by: the fields may arrive
/// in any order and either may be absent.
#[derive(Default, Clone, Copy)]
struct LevelBuild {
    kind: Option<ListKind>,
    start: Option<u32>,
    /// `w:startOverride`, which prevails over any nested `w:lvl/w:start`.
    start_override: Option<u32>,
    /// `w:lvlRestart`: the 1-based level whose advance restarts this one;
    /// 0 means never restart.
    restart: Option<u32>,
}

/// The numbering walk's state while the part streams by.
#[derive(Default)]
struct NumberingWalk {
    abstracts: BTreeMap<String, BTreeMap<usize, LevelBuild>>,
    /// Per-instance `w:lvlOverride` definitions, keyed by numId.
    overrides: BTreeMap<String, BTreeMap<usize, LevelBuild>>,
    nums: BTreeMap<String, String>,
    style_levels: BTreeMap<String, BTreeMap<String, usize>>,
    current_abstract: Option<String>,
    current_level: Option<usize>,
    current_num: Option<String>,
    /// The level a `w:lvlOverride` names, while one is open.
    current_override: Option<usize>,
}

impl NumberingWalk {
    /// The level slot the current context writes to: an open
    /// `w:lvlOverride` routes into the instance overrides, an open
    /// `w:abstractNum` level into the abstract definitions.
    fn level_slot(&mut self) -> Option<&mut LevelBuild> {
        if let (Some(num_id), Some(level)) = (self.current_num.as_ref(), self.current_override) {
            return Some(
                self.overrides
                    .entry(num_id.clone())
                    .or_default()
                    .entry(level)
                    .or_default(),
            );
        }
        if let (Some(abstract_id), Some(level)) =
            (self.current_abstract.as_ref(), self.current_level)
        {
            return Some(
                self.abstracts
                    .entry(abstract_id.clone())
                    .or_default()
                    .entry(level)
                    .or_default(),
            );
        }
        None
    }
}

impl AuxiliarySink for NumberingWalk {
    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError> {
        numbering_element(reader, start, self)
    }

    /// A closing container leaves its context: its levels stop receiving
    /// fields.
    fn close(&mut self, local_name: &str) {
        match local_name {
            "abstractNum" => self.current_abstract = None,
            "lvl" => self.current_level = None,
            "lvlOverride" => self.current_override = None,
            "num" => self.current_num = None,
            _ => {}
        }
    }
}

/// The numbering part: numId → (list level → kind) with the `w:num` →
/// `w:abstractNum` indirection resolved, plus the paragraph styles that
/// levels associate to. Level overrides (`w:lvlOverride`) are not applied
/// in v1.
fn numbering_part(xml: &str) -> Result<NumberingPart, DocxError> {
    let mut walk = NumberingWalk::default();
    walk_auxiliary(xml, PartFrame::new("numbering", "numbering"), &mut walk)?;

    let empty = BTreeMap::new();
    let mut kinds = Numbering::new();
    for (num_id, abstract_id) in &walk.nums {
        let base = match walk.abstracts.get(abstract_id) {
            Some(levels) => levels,
            None => &empty,
        };
        let over = match walk.overrides.get(num_id) {
            Some(levels) => levels,
            None => &empty,
        };
        let mut levels = BTreeMap::new();
        for level in base.keys().chain(over.keys()) {
            let kind = over
                .get(level)
                .and_then(|build| build.kind)
                .or_else(|| base.get(level).and_then(|build| build.kind));
            // `w:startOverride` prevails over any `w:lvl/w:start`, which
            // prevails over the abstract definition's.
            let start = over
                .get(level)
                .and_then(|build| build.start_override)
                .or_else(|| over.get(level).and_then(|build| build.start))
                .or_else(|| base.get(level).and_then(|build| build.start));
            let restart = over
                .get(level)
                .and_then(|build| build.restart)
                .or_else(|| base.get(level).and_then(|build| build.restart));
            let def = match (kind, start) {
                (Some(kind), Some(start)) => Some(LevelDef {
                    kind,
                    start,
                    restart,
                }),
                // An omitted `w:start` defaults to 0 per the OOXML schema
                // (StartNumberingValue), not the 1 Word's templates write.
                (Some(kind), None) => Some(LevelDef {
                    kind,
                    start: 0,
                    restart,
                }),
                (None, _) => None,
            };
            if let Some(def) = def {
                levels.insert(*level, def);
            }
        }
        kinds.insert(num_id.clone(), levels);
    }
    Ok(NumberingPart {
        kinds,
        abstracts: walk.nums,
        style_levels: walk.style_levels,
    })
}

/// One element inside the numbering part: abstract definitions, their
/// per-level formats and style associations, and the num → abstractNum
/// bindings.
fn numbering_element<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    walk: &mut NumberingWalk,
) -> Result<(), DocxError> {
    match start.local_name().as_ref() {
        "abstractNum" => {
            walk.current_abstract = attribute(reader, start, "abstractNumId")?;
        }
        "lvl" => walk.current_level = declared_level(reader, start)?,
        "lvlOverride" => walk.current_override = declared_level(reader, start)?,
        "numFmt" => {
            if let Some(format) = attribute(reader, start, "val")?
                && let Some(slot) = walk.level_slot()
            {
                slot.kind = Some(list_kind(&format));
            }
        }
        "start" => {
            if let Some(value) = attribute(reader, start, "val")? {
                let starts_at = list_start(&value)?;
                if let Some(slot) = walk.level_slot() {
                    slot.start = Some(starts_at);
                }
            }
        }
        "lvlRestart" => {
            if let Some(value) = attribute(reader, start, "val")? {
                let restart = list_start(&value)?;
                if let Some(slot) = walk.level_slot() {
                    slot.restart = Some(restart);
                }
            }
        }
        // `w:startOverride` resets an instance's numbering start and
        // prevails over any nested `w:lvl/w:start`.
        "startOverride" => {
            if let Some(value) = attribute(reader, start, "val")? {
                let starts_at = list_start(&value)?;
                if let Some(slot) = walk.level_slot() {
                    slot.start_override = Some(starts_at);
                }
            }
        }
        // `w:lvl > w:pStyle` associates a paragraph style with this level,
        // scoped to the containing abstract definition: style-applied
        // lists take their level from here.
        "pStyle" => {
            if let (Some(abstract_id), Some(level), Some(style)) = (
                walk.current_abstract.as_ref(),
                walk.current_level,
                attribute(reader, start, "val")?,
            ) {
                walk.style_levels
                    .entry(abstract_id.clone())
                    .or_default()
                    .insert(style, level);
            }
        }
        "num" => walk.current_num = attribute(reader, start, "numId")?,
        "abstractNumId" => {
            if let (Some(num_id), Some(value)) =
                (walk.current_num.as_ref(), attribute(reader, start, "val")?)
            {
                walk.nums.insert(num_id.clone(), value);
            }
        }
        _ => {}
    }
    Ok(())
}

/// The marker kind an `ST_NumberFormat` value names: `bullet` bullets,
/// `none` marks nothing — Word renders such levels without a marker and
/// the projection must not invent one — and every other format numbers.
fn list_kind(format: &str) -> ListKind {
    match format {
        "bullet" => ListKind::Bullet,
        "none" => ListKind::Unmarked,
        _ => ListKind::Ordered,
    }
}

/// The level a `w:lvl` or `w:lvlOverride` names (`w:ilvl`), bounded to
/// Word's 0..=8; an absent attribute names none.
fn declared_level<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
) -> Result<Option<usize>, DocxError> {
    match attribute(reader, start, "ilvl")? {
        Some(value) => Ok(Some(list_level(&value)?)),
        None => Ok(None),
    }
}

/// A table as markdown rows, with the separator markdown requires after the
/// first row. Rows a pending revision deletes are not part of the accepted
/// body; a table with nothing left renders nothing.
fn render_table(table: &Table) -> Option<String> {
    let mut rows = table.rows.iter().filter(|row| !row.deleted);
    let first = rows.next()?;
    let mut lines = vec![table_row(&first.cells)];
    lines.push(format!(
        "|{}",
        " --- |".repeat(first.cells.iter().filter(|cell| !cell.deleted).count())
    ));
    lines.extend(rows.map(|row| table_row(&row.cells)));
    Some(lines.join("\n"))
}

fn table_row(cells: &[Cell]) -> String {
    let rendered: Vec<String> = cells
        .iter()
        .filter(|cell| !cell.deleted)
        .map(|cell| format!(" {} ", encoded_cell(cell)))
        .collect();
    format!("|{}|", rendered.join("|"))
}

/// Cell text on one markdown row line: backslashes escape first so the
/// escapes stay injective, pipes cannot break the row, hard breaks render
/// as the two-character sequence `\n`, and paragraph boundaries as `\p`
/// — neither can collide with a literal space or with each other.
fn encoded_cell(cell: &Cell) -> String {
    let encoded: Vec<String> = cell
        .paragraphs
        .iter()
        .map(|paragraph| {
            paragraph
                .replace('\\', "\\\\")
                .replace('|', "\\|")
                .replace('\n', "\\n")
        })
        .collect();
    encoded.join("\\p")
}

/// The replacement text of `&name;` and `&#N;` references: character
/// references and the five predefined XML entities, the replacement
/// checked against the XML character set. A docx declares no other
/// entities, so anything else is malformed rather than droppable.
fn resolve_reference(reference: &BytesRef<'_>) -> Result<String, DocxError> {
    let replacement = if let Some(resolved) = reference.resolve_char_ref()? {
        resolved.to_string()
    } else {
        let name = reference.xml10_content();
        match resolve_xml_entity(&name) {
            Some(replacement) => replacement.to_owned(),
            None => {
                return Err(DocxError::Structure(format!(
                    "unresolvable entity reference &{name};"
                )));
            }
        }
    };
    ensure_xml_chars(&replacement)?;
    Ok(replacement)
}

/// The value of the `name` attribute on `start`, matched by local name for
/// attributes in no namespace or the `WordprocessingML` one. Every
/// attribute's prefix must resolve — an undeclared prefix is malformed
/// XML, and accepting it would project silently wrong markdown.
pub(crate) fn attribute<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    name: &str,
) -> Result<Option<String>, DocxError> {
    let mut found = None;
    for attribute in start.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        if attribute.key.as_namespace_binding().is_some() {
            continue;
        }
        let (resolve, local) = reader.resolver().resolve_attribute(attribute.key);
        let resolve = checked(resolve)?;
        if found.is_none()
            && local.as_ref() == name
            && (matches!(resolve, ResolveResult::Unbound) || in_wordprocessingml(&resolve))
        {
            found = Some(
                attribute
                    .normalized_value(XmlVersion::Implicit1_0)?
                    .into_owned(),
            );
        }
    }
    Ok(found)
}
