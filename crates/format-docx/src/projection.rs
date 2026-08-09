use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesRef, BytesStart, Event};
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
const RELATIONSHIPS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";

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
const MAX_PART_SIZE: u64 = 64 * 1024 * 1024;

/// The namespaces `WordprocessingML` elements live in: the transitional one
/// Word writes, and the ISO strict variant.
const WORDPROCESSINGML: [&[u8]; 2] = [
    b"http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    b"http://purl.oclc.org/ooxml/wordprocessingml/main",
];

/// The markup-compatibility namespace: its `Choice` branches carry content
/// for consumers that support some extension, its `Fallback` the
/// equivalent for those that do not.
const MARKUP_COMPATIBILITY: &[u8] = b"http://schemas.openxmlformats.org/markup-compatibility/2006";

/// Markdown headings stop at six `#`s; deeper Word headings clamp to it.
const MAX_HEADING_LEVEL: usize = 6;

/// Word's list levels run 0..=8 (`w:ilvl`).
const MAX_LIST_LEVEL: usize = 8;

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
const SKIPPED_SUBTREES: [&[u8]; 10] = [
    b"pPrChange",
    b"rPrChange",
    b"tblPrChange",
    b"trPrChange",
    b"tcPrChange",
    b"sectPrChange",
    b"numberingChange",
    b"del",
    b"moveFrom",
    b"txbxContent",
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
    // OPC locates parts through relationships, not fixed paths, and the
    // relationship graph is authoritative: when a relationships part
    // exists, only what it names counts. The canonical layout applies
    // only to packages whose relationship parts are absent — the minimal
    // producers that omit them always use it.
    let document_path = match part(&mut archive, PACKAGE_RELS)? {
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
    let Some(document) = part(&mut archive, &document_path)? else {
        return Err(DocxError::MissingDocument);
    };
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
    let mut frame = PartFrame::new("relationships", b"Relationships");
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
                let content = text.xml10_content().map_err(quick_xml::Error::from)?;
                ensure_xml_chars(&content)?;
                frame.characters(xml_whitespace_only(&content))?;
            }
            (_, Event::CData(data)) => {
                let content = std::str::from_utf8(&data).map_err(|_| {
                    DocxError::Structure("CDATA content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
                frame.characters(false)?;
            }
            (_, Event::GeneralRef(reference)) => {
                ensure_xml_chars(&resolve_reference(&reference)?)?;
                frame.characters(false)?;
            }
            (_, Event::Decl(_) | Event::DocType(_)) => {
                frame.prolog_declaration()?;
            }
            (_, Event::Comment(text)) => {
                let content = std::str::from_utf8(&text).map_err(|_| {
                    DocxError::Structure("comment content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::PI(instruction)) => {
                let content = std::str::from_utf8(&instruction).map_err(|_| {
                    DocxError::Structure(
                        "processing-instruction content is not valid UTF-8".to_owned(),
                    )
                })?;
                ensure_xml_chars(content)?;
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
    if start.local_name().as_ref() != b"Relationship" || found.is_some() {
        return Ok(());
    }
    let Some(kind) = unqualified_attribute(reader, start, b"Type")? else {
        return Ok(());
    };
    if !types.contains(&kind.as_str()) {
        return Ok(());
    }
    if let Some(mode) = unqualified_attribute(reader, start, b"TargetMode")?
        && mode == "External"
    {
        return Ok(());
    }
    if let Some(target) = unqualified_attribute(reader, start, b"Target")? {
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
    name: &[u8],
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
    part.take(MAX_PART_SIZE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PART_SIZE {
        return Err(DocxError::Structure(format!(
            "{name} inflates past {MAX_PART_SIZE} bytes"
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
    root: &'static [u8],
    depth: usize,
    root_closed: bool,
}

impl PartFrame {
    fn new(label: &'static str, root: &'static [u8]) -> Self {
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
    fn empty(&mut self, wordprocessingml: bool, local_name: &[u8]) -> Result<(), DocxError> {
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

    fn end(&mut self, wordprocessingml: bool, local_name: &[u8]) -> Result<(), DocxError> {
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
    let mut reader = NsReader::from_str(xml);
    let mut document = Document::new(styles, default_style, numbering);
    let mut frame = PartFrame::new("document", b"document");
    // The depth a masked subtree opened at, while one is open. Two kinds
    // mask: `mc:Choice` branches (this projector supports no extensions,
    // so only the Fallback projects) and text-box containers in ANY
    // namespace — strict documents wrap them as `wne:txbxContent`, and
    // descending into their paragraphs would corrupt the body walk.
    let mut choice_at: Option<usize> = None;

    loop {
        match reader.read_resolved_event()? {
            (resolve, Event::Start(start)) => {
                frame.start()?;
                let resolve = checked(resolve)?;
                let wordprocessingml = in_wordprocessingml(&resolve);
                if choice_at.is_none()
                    && (in_markup_compatibility(&resolve)
                        && start.local_name().as_ref() == b"Choice"
                        || start.local_name().as_ref() == b"txbxContent")
                {
                    choice_at = Some(frame.depth);
                }
                resolved_attributes(&reader, &start)?;
                if wordprocessingml && choice_at.is_none() {
                    document.open(&reader, &start)?;
                }
            }
            (resolve, Event::Empty(start)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.empty(wordprocessingml, start.local_name().as_ref())?;
                resolved_attributes(&reader, &start)?;
                if wordprocessingml && choice_at.is_none() {
                    document.open(&reader, &start)?;
                    document.close(start.local_name().as_ref())?;
                }
            }
            (resolve, Event::End(end)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.end(wordprocessingml, end.local_name().as_ref())?;
                if let Some(depth) = choice_at {
                    if frame.depth < depth {
                        choice_at = None;
                    }
                    continue;
                }
                if wordprocessingml {
                    document.close(end.local_name().as_ref())?;
                }
            }
            (_, Event::Text(text)) => {
                let content = text.xml10_content().map_err(quick_xml::Error::from)?;
                ensure_xml_chars(&content)?;
                frame.characters(xml_whitespace_only(&content))?;
                if choice_at.is_none() {
                    document.text(&normalized_eol(&content))?;
                }
            }
            (_, Event::CData(data)) => {
                let content = std::str::from_utf8(&data).map_err(|_| {
                    DocxError::Structure("CDATA content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
                frame.characters(false)?;
                if choice_at.is_none() {
                    document.text(&normalized_eol(content))?;
                }
            }
            (_, Event::GeneralRef(reference)) => {
                frame.characters(false)?;
                let content = resolve_reference(&reference)?;
                ensure_xml_chars(&content)?;
                if choice_at.is_none() {
                    document.text(&content)?;
                }
            }
            (_, Event::Decl(_) | Event::DocType(_)) => {
                frame.prolog_declaration()?;
            }
            (_, Event::Comment(text)) => {
                let content = std::str::from_utf8(&text).map_err(|_| {
                    DocxError::Structure("comment content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::PI(instruction)) => {
                let content = std::str::from_utf8(&instruction).map_err(|_| {
                    DocxError::Structure(
                        "processing-instruction content is not valid UTF-8".to_owned(),
                    )
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::Eof) => {
                frame.eof()?;
                break;
            }
        }
    }

    document.ensure_closed()?;
    Ok(document.render())
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
            "undeclared namespace prefix {}",
            String::from_utf8_lossy(prefix)
        )));
    }
    Ok(resolve)
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

#[derive(Default)]
struct Paragraph {
    style: Option<String>,
    outline: Option<usize>,
    listed: bool,
    num_id: Option<String>,
    level: usize,
    text: String,
    /// A tracked deletion of the paragraph mark: accepting it merges this
    /// paragraph's text into the next.
    mark_deleted: bool,
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

    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError> {
        if self.skipping > 0 {
            self.skipping += 1;
            return Ok(());
        }
        let name = start.local_name();
        if SKIPPED_SUBTREES.contains(&name.as_ref()) {
            if name.as_ref() == b"del" {
                if self.in_row_properties {
                    if let Some(row) = self
                        .tables
                        .last_mut()
                        .and_then(|table| table.rows.last_mut())
                    {
                        row.deleted = true;
                    }
                } else if self.in_properties {
                    // A deleted paragraph mark (w:pPr/w:rPr/w:del): the
                    // accepted body merges this paragraph into the next.
                    if let Some(paragraph) = self.paragraph.as_mut() {
                        paragraph.mark_deleted = true;
                    }
                }
            }
            self.skipping = 1;
            return Ok(());
        }
        match name.as_ref() {
            b"p" => self.paragraph = Some(Paragraph::default()),
            b"pPr" => self.in_properties = true,
            b"trPr" => self.in_row_properties = true,
            b"tcPr" => self.in_cell_properties = true,
            // A tracked cell deletion: the cell leaves the accepted body,
            // like rows marked deleted in their trPr.
            b"cellDel" if self.in_cell_properties => {
                if let Some(cell) = self
                    .tables
                    .last_mut()
                    .and_then(|table| table.rows.last_mut())
                    .and_then(|row| row.cells.last_mut())
                {
                    cell.deleted = true;
                }
            }
            b"pStyle" if self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.style = attribute(reader, start, b"val")?;
                }
            }
            b"outlineLvl" if self.in_properties => {
                if let (Some(paragraph), Some(value)) =
                    (self.paragraph.as_mut(), attribute(reader, start, b"val")?)
                {
                    paragraph.outline = Some(outline_level(&value)?);
                }
            }
            b"numPr" if self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.listed = true;
                }
            }
            b"numId" if self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.num_id = attribute(reader, start, b"val")?;
                }
            }
            b"ilvl" if self.in_properties => {
                if let (Some(paragraph), Some(level)) =
                    (self.paragraph.as_mut(), attribute(reader, start, b"val")?)
                {
                    paragraph.level = list_level(&level)?;
                }
            }
            b"t" => self.in_text = true,
            b"tab" if !self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.text.push('\t');
                }
            }
            b"br" | b"cr" => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.text.push('\n');
                }
            }
            // Visible inline characters Word encodes as elements: erasing
            // them would project distinct sentences identically.
            b"sym" if !self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    let value = attribute(reader, start, b"char")?.ok_or_else(|| {
                        DocxError::Structure("sym without a char attribute".to_owned())
                    })?;
                    paragraph.text.push(sym_char(&value)?);
                }
            }
            b"noBreakHyphen" if !self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.text.push('\u{2011}');
                }
            }
            b"softHyphen" if !self.in_properties => {
                if let Some(paragraph) = self.paragraph.as_mut() {
                    paragraph.text.push('\u{00ad}');
                }
            }
            b"tbl" => self.tables.push(Table::default()),
            b"tr" => match self.tables.last_mut() {
                Some(table) => table.rows.push(Row::default()),
                None => {
                    return Err(DocxError::Structure("table row outside a table".to_owned()));
                }
            },
            b"tc" => match self
                .tables
                .last_mut()
                .and_then(|table| table.rows.last_mut())
            {
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

    fn close(&mut self, local_name: &[u8]) -> Result<(), DocxError> {
        if self.skipping > 0 {
            self.skipping -= 1;
            return Ok(());
        }
        match local_name {
            b"pPr" => self.in_properties = false,
            b"trPr" => self.in_row_properties = false,
            b"tcPr" => self.in_cell_properties = false,
            b"t" => self.in_text = false,
            b"p" => {
                let mut paragraph = self.paragraph.take().ok_or_else(|| {
                    DocxError::Structure("paragraph end without a paragraph".to_owned())
                })?;
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
            b"body" => {
                if let Some(carried) = self.carry.take() {
                    self.finish_paragraph(&Paragraph {
                        text: carried,
                        ..Paragraph::default()
                    })?;
                }
            }
            b"tbl" => {
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
                paragraph.text.push_str(content);
                Ok(())
            }
            None => Err(DocxError::Structure("text outside a paragraph".to_owned())),
        }
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
            // A parent item advancing — whatever its marker kind —
            // restarts deeper ordered levels, each per its own
            // `w:lvlRestart` policy.
            let advancing = *level;
            let doomed: Vec<(String, usize)> = self
                .ordinals
                .keys()
                .filter(|(owner, deeper)| owner == id && *deeper > advancing)
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
            return (outline != OUTLINE_BODY_TEXT).then(|| (outline + 1).min(MAX_HEADING_LEVEL));
        }
        let style = paragraph.style.as_deref().or(self.default_style)?;
        if let Some(level) = heading_level(style) {
            return Some(level);
        }
        self.styles
            .get(style)?
            .outline
            .map(|outline| (outline + 1).min(MAX_HEADING_LEVEL))
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
        let cell = self
            .tables
            .last_mut()
            .and_then(|table| table.rows.last_mut())
            .and_then(|row| row.cells.last_mut())
            .ok_or_else(|| {
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

/// Plain body text whose lines begin with markdown structure — `# `,
/// `1. `, `- `, `>`, `|`, a thematic break — escapes the marker, so a
/// paragraph that merely *says* "1. Scope" stays distinguishable from a
/// real list item and the two can never project identically.
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
    // Literal backslashes escape first, so a generated escape can never
    // collide with one the document text already contained — the mapping
    // from body text to markdown stays injective.
    let line = line.replace('\\', "\\\\");
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return line;
    };
    let second = chars.next();
    let escapes = match first {
        '#' | '>' | '|' | '`' | '~' => true,
        '-' | '+' | '*' | '_' => second.is_none_or(|c| c == ' ' || c == '\t' || c == first),
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
        return line;
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
fn sym_char(value: &str) -> Result<char, DocxError> {
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
    Some(level.min(MAX_HEADING_LEVEL))
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
    if level > MAX_LIST_LEVEL {
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

/// What each paragraph style contributes, `basedOn` chains followed:
/// outline levels for headings, and `numPr` numbering for lists applied
/// through styles. Historical `*Change` records inside style definitions
/// are excluded, as in the document walk.
fn resolved_styles(
    xml: &str,
) -> Result<(BTreeMap<String, ResolvedStyle>, Option<String>), DocxError> {
    let mut reader = NsReader::from_str(xml);
    let mut frame = PartFrame::new("styles", b"styles");
    let mut styles: BTreeMap<String, StyleDef> = BTreeMap::new();
    let mut current: Option<(String, StyleDef)> = None;
    let mut default_style: Option<String> = None;
    let mut skipping: usize = 0;
    // Choice branches carry extension content this projector does not
    // support; only the Fallback branch contributes (as in the document
    // walk).
    let mut choice_at: Option<usize> = None;

    loop {
        match reader.read_resolved_event()? {
            (resolve, Event::Start(start)) => {
                frame.start()?;
                let resolve = checked(resolve)?;
                let wordprocessingml = in_wordprocessingml(&resolve);
                if choice_at.is_none()
                    && in_markup_compatibility(&resolve)
                    && start.local_name().as_ref() == b"Choice"
                {
                    choice_at = Some(frame.depth);
                }
                resolved_attributes(&reader, &start)?;
                if !wordprocessingml || choice_at.is_some() {
                    continue;
                }
                if skipping > 0 {
                    skipping += 1;
                    continue;
                }
                if SKIPPED_SUBTREES.contains(&start.local_name().as_ref()) {
                    skipping = 1;
                    continue;
                }
                style_element(&reader, &start, &mut current, &mut default_style)?;
            }
            (resolve, Event::Empty(start)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.empty(wordprocessingml, start.local_name().as_ref())?;
                resolved_attributes(&reader, &start)?;
                if !wordprocessingml || skipping > 0 || choice_at.is_some() {
                    continue;
                }
                if SKIPPED_SUBTREES.contains(&start.local_name().as_ref()) {
                    continue;
                }
                style_element(&reader, &start, &mut current, &mut default_style)?;
            }
            (resolve, Event::End(end)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.end(wordprocessingml, end.local_name().as_ref())?;
                if let Some(depth) = choice_at {
                    if frame.depth < depth {
                        choice_at = None;
                    }
                    continue;
                }
                if !wordprocessingml {
                    continue;
                }
                if skipping > 0 {
                    skipping -= 1;
                    continue;
                }
                if end.local_name().as_ref() == b"style"
                    && let Some((id, def)) = current.take()
                {
                    styles.insert(id, def);
                }
            }
            (_, Event::Text(text)) => {
                let content = text.xml10_content().map_err(quick_xml::Error::from)?;
                ensure_xml_chars(&content)?;
                frame.characters(xml_whitespace_only(&content))?;
            }
            (_, Event::CData(data)) => {
                let content = std::str::from_utf8(&data).map_err(|_| {
                    DocxError::Structure("CDATA content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
                frame.characters(false)?;
            }
            (_, Event::GeneralRef(reference)) => {
                ensure_xml_chars(&resolve_reference(&reference)?)?;
                frame.characters(false)?;
            }
            (_, Event::Decl(_) | Event::DocType(_)) => {
                frame.prolog_declaration()?;
            }
            (_, Event::Comment(text)) => {
                let content = std::str::from_utf8(&text).map_err(|_| {
                    DocxError::Structure("comment content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::PI(instruction)) => {
                let content = std::str::from_utf8(&instruction).map_err(|_| {
                    DocxError::Structure(
                        "processing-instruction content is not valid UTF-8".to_owned(),
                    )
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::Eof) => {
                frame.eof()?;
                break;
            }
        }
    }

    let chains = resolved_chains(&styles)?;
    let mut resolved = BTreeMap::new();
    for (id, chain) in chains {
        let outline = match chain.outline {
            Some(level) if level != OUTLINE_BODY_TEXT => Some(level),
            _ => None,
        };
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
    Ok((resolved, default_style))
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

/// One element inside a style definition: the properties a style
/// contributes to paragraphs that name it.
fn style_element<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    current: &mut Option<(String, StyleDef)>,
    default_style: &mut Option<String>,
) -> Result<(), DocxError> {
    match start.local_name().as_ref() {
        b"style" => {
            if let Some(id) = attribute(reader, start, b"styleId")? {
                // The default paragraph style applies to paragraphs that
                // name no style at all, as Word applies it.
                let kind = attribute(reader, start, b"type")?;
                let default = attribute(reader, start, b"default")?;
                if default_style.is_none()
                    && kind.as_deref() == Some("paragraph")
                    && matches!(default.as_deref(), Some("1" | "true" | "on"))
                {
                    *default_style = Some(id.clone());
                }
                *current = Some((
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
        b"basedOn" => {
            if let Some((_, def)) = current.as_mut() {
                def.based_on = attribute(reader, start, b"val")?;
            }
        }
        b"outlineLvl" => {
            if let (Some((_, def)), Some(value)) =
                (current.as_mut(), attribute(reader, start, b"val")?)
            {
                def.outline = Some(outline_level(&value)?);
            }
        }
        b"numId" => {
            if let Some((_, def)) = current.as_mut() {
                def.num_id = attribute(reader, start, b"val")?;
            }
        }
        b"ilvl" => {
            if let (Some((_, def)), Some(value)) =
                (current.as_mut(), attribute(reader, start, b"val")?)
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

/// The numbering part: numId → (list level → kind) with the `w:num` →
/// `w:abstractNum` indirection resolved, plus the paragraph styles that
/// levels associate to. Level overrides (`w:lvlOverride`) are not applied
/// in v1.
fn numbering_part(xml: &str) -> Result<NumberingPart, DocxError> {
    let mut reader = NsReader::from_str(xml);
    let mut frame = PartFrame::new("numbering", b"numbering");
    let mut walk = NumberingWalk::default();
    let mut skipping: usize = 0;
    // Choice branches carry extension content this projector does not
    // support; only the Fallback branch contributes (as in the document
    // walk).
    let mut choice_at: Option<usize> = None;

    loop {
        match reader.read_resolved_event()? {
            (resolve, Event::Start(start)) => {
                frame.start()?;
                let resolve = checked(resolve)?;
                let wordprocessingml = in_wordprocessingml(&resolve);
                if choice_at.is_none()
                    && in_markup_compatibility(&resolve)
                    && start.local_name().as_ref() == b"Choice"
                {
                    choice_at = Some(frame.depth);
                }
                resolved_attributes(&reader, &start)?;
                if !wordprocessingml || choice_at.is_some() {
                    continue;
                }
                if skipping > 0 {
                    skipping += 1;
                    continue;
                }
                if SKIPPED_SUBTREES.contains(&start.local_name().as_ref()) {
                    skipping = 1;
                    continue;
                }
                numbering_element(&reader, &start, &mut walk)?;
            }
            (resolve, Event::Empty(start)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.empty(wordprocessingml, start.local_name().as_ref())?;
                resolved_attributes(&reader, &start)?;
                if !wordprocessingml || skipping > 0 || choice_at.is_some() {
                    continue;
                }
                if SKIPPED_SUBTREES.contains(&start.local_name().as_ref()) {
                    continue;
                }
                numbering_element(&reader, &start, &mut walk)?;
            }
            (resolve, Event::End(end)) => {
                let wordprocessingml = in_wordprocessingml(&checked(resolve)?);
                frame.end(wordprocessingml, end.local_name().as_ref())?;
                if let Some(depth) = choice_at {
                    if frame.depth < depth {
                        choice_at = None;
                    }
                    continue;
                }
                if !wordprocessingml {
                    continue;
                }
                if skipping > 0 {
                    skipping -= 1;
                    continue;
                }
                match end.local_name().as_ref() {
                    b"abstractNum" => walk.current_abstract = None,
                    b"lvl" => walk.current_level = None,
                    b"lvlOverride" => walk.current_override = None,
                    b"num" => walk.current_num = None,
                    _ => {}
                }
            }
            (_, Event::Text(text)) => {
                let content = text.xml10_content().map_err(quick_xml::Error::from)?;
                ensure_xml_chars(&content)?;
                frame.characters(xml_whitespace_only(&content))?;
            }
            (_, Event::CData(data)) => {
                let content = std::str::from_utf8(&data).map_err(|_| {
                    DocxError::Structure("CDATA content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
                frame.characters(false)?;
            }
            (_, Event::GeneralRef(reference)) => {
                ensure_xml_chars(&resolve_reference(&reference)?)?;
                frame.characters(false)?;
            }
            (_, Event::Decl(_) | Event::DocType(_)) => {
                frame.prolog_declaration()?;
            }
            (_, Event::Comment(text)) => {
                let content = std::str::from_utf8(&text).map_err(|_| {
                    DocxError::Structure("comment content is not valid UTF-8".to_owned())
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::PI(instruction)) => {
                let content = std::str::from_utf8(&instruction).map_err(|_| {
                    DocxError::Structure(
                        "processing-instruction content is not valid UTF-8".to_owned(),
                    )
                })?;
                ensure_xml_chars(content)?;
            }
            (_, Event::Eof) => {
                frame.eof()?;
                break;
            }
        }
    }

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
        b"abstractNum" => {
            walk.current_abstract = attribute(reader, start, b"abstractNumId")?;
        }
        b"lvl" => {
            walk.current_level = match attribute(reader, start, b"ilvl")? {
                Some(value) => Some(list_level(&value)?),
                None => None,
            };
        }
        b"lvlOverride" => {
            walk.current_override = match attribute(reader, start, b"ilvl")? {
                Some(value) => Some(list_level(&value)?),
                None => None,
            };
        }
        b"numFmt" => {
            if let Some(format) = attribute(reader, start, b"val")? {
                let kind = match format.as_str() {
                    "bullet" => ListKind::Bullet,
                    // Word renders a `none` level without a marker; the
                    // projection must not invent one.
                    "none" => ListKind::Unmarked,
                    _ => ListKind::Ordered,
                };
                if let Some(slot) = walk.level_slot() {
                    slot.kind = Some(kind);
                }
            }
        }
        b"start" => {
            if let Some(value) = attribute(reader, start, b"val")? {
                let starts_at = list_start(&value)?;
                if let Some(slot) = walk.level_slot() {
                    slot.start = Some(starts_at);
                }
            }
        }
        b"lvlRestart" => {
            if let Some(value) = attribute(reader, start, b"val")? {
                let restart = list_start(&value)?;
                if let Some(slot) = walk.level_slot() {
                    slot.restart = Some(restart);
                }
            }
        }
        // `w:startOverride` resets an instance's numbering start and
        // prevails over any nested `w:lvl/w:start`.
        b"startOverride" => {
            if let Some(value) = attribute(reader, start, b"val")? {
                let starts_at = list_start(&value)?;
                if let Some(slot) = walk.level_slot() {
                    slot.start_override = Some(starts_at);
                }
            }
        }
        // `w:lvl > w:pStyle` associates a paragraph style with this level,
        // scoped to the containing abstract definition: style-applied
        // lists take their level from here.
        b"pStyle" => {
            if let (Some(abstract_id), Some(level), Some(style)) = (
                walk.current_abstract.as_ref(),
                walk.current_level,
                attribute(reader, start, b"val")?,
            ) {
                walk.style_levels
                    .entry(abstract_id.clone())
                    .or_default()
                    .insert(style, level);
            }
        }
        b"num" => walk.current_num = attribute(reader, start, b"numId")?,
        b"abstractNumId" => {
            if let (Some(num_id), Some(value)) =
                (walk.current_num.as_ref(), attribute(reader, start, b"val")?)
            {
                walk.nums.insert(num_id.clone(), value);
            }
        }
        _ => {}
    }
    Ok(())
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
/// references and the five predefined XML entities. A docx declares no
/// other entities, so anything else is malformed rather than droppable.
fn resolve_reference(reference: &BytesRef<'_>) -> Result<String, DocxError> {
    if let Some(resolved) = reference.resolve_char_ref()? {
        return Ok(resolved.to_string());
    }
    let name = reference.xml10_content().map_err(quick_xml::Error::from)?;
    match resolve_xml_entity(&name) {
        Some(replacement) => Ok(replacement.to_owned()),
        None => Err(DocxError::Structure(format!(
            "unresolvable entity reference &{name};"
        ))),
    }
}

/// The value of the `name` attribute on `start`, matched by local name for
/// attributes in no namespace or the `WordprocessingML` one. Every
/// attribute's prefix must resolve — an undeclared prefix is malformed
/// XML, and accepting it would project silently wrong markdown.
fn attribute<R>(
    reader: &NsReader<R>,
    start: &BytesStart<'_>,
    name: &[u8],
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
