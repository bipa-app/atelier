//! The docx differ: rich deltas for formatting markdown cannot express.
//!
//! The Rich rung is additive (ADR-0003): text changes are the text rung's
//! story, told by the projection line diff, so this differ reports only
//! run-property changes on paragraphs whose text is unchanged — font size,
//! family, color, underline, and the emphasis trio when it co-occurs with
//! one of those. An emphasis-only change already shows at the text rung as
//! markdown markers and produces no delta here.

use std::io::Cursor;

use atelier_diff_core::{Address, Delta, PackageId};
use quick_xml::events::BytesStart;
use quick_xml::reader::NsReader;
use similar::{Algorithm, DiffOp, capture_diff_slices};
use zip::ZipArchive;

use crate::projection::{
    BodySink, DocxError, SKIPPED_SUBTREES, attribute, document_part, on_off, sym_char, walk_body,
};

/// The run properties this differ models. Everything here is direct `rPr`
/// formatting; styles-applied formatting is not modeled in v1, matching
/// the projector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RunProps {
    bold: bool,
    italic: bool,
    strike: bool,
    /// The `w:u` value, `None` when absent or explicitly `none` — both
    /// mean no underline, and Word writes either.
    underline: Option<String>,
    /// The `w:sz` value in half-points, kept as written and validated
    /// numeric when described.
    size: Option<String>,
    /// The `w:rFonts` `w:ascii` font; the other script slots are not
    /// modeled in v1.
    family: Option<String>,
    /// The `w:color` value as written (`auto` included).
    color: Option<String>,
}

/// One maximal stretch of characters sharing one set of run properties.
struct Span {
    chars: usize,
    props: RunProps,
}

/// One paragraph of the accepted body — wherever it sits, body or table
/// cell — as the differ models it: its text and each character's props.
#[derive(Default)]
struct Block {
    text: String,
    spans: Vec<Span>,
}

impl Block {
    fn append(&mut self, content: &str, props: &RunProps) {
        if content.is_empty() {
            return;
        }
        let count = content.chars().count();
        self.text.push_str(content);
        match self.spans.last_mut() {
            Some(span) if span.props == *props => span.chars += count,
            _ => self.spans.push(Span {
                chars: count,
                props: props.clone(),
            }),
        }
    }

    /// Each character's properties, in order.
    fn char_props(&self) -> impl Iterator<Item = &RunProps> {
        self.spans
            .iter()
            .flat_map(|span| std::iter::repeat_n(&span.props, span.chars))
    }
}

/// The differ's body walk: paragraphs with per-character run properties,
/// in document order. Tracked deletions and pre-revision properties are
/// excluded like the projector excludes them; a paragraph-mark deletion's
/// merge is not modeled — such an edit changes paragraph texts, which the
/// text rung reports.
#[derive(Default)]
struct BlockWalk {
    blocks: Vec<Block>,
    paragraph: Option<Block>,
    run: RunProps,
    in_paragraph_properties: bool,
    in_run_properties: bool,
    in_text: bool,
    skipping: usize,
}

impl BodySink for BlockWalk {
    fn open<R>(&mut self, reader: &NsReader<R>, start: &BytesStart<'_>) -> Result<(), DocxError> {
        if self.skipping > 0 {
            self.skipping += 1;
            return Ok(());
        }
        let name = start.local_name();
        if SKIPPED_SUBTREES.contains(&name.as_ref()) {
            self.skipping = 1;
            return Ok(());
        }
        match name.as_ref() {
            b"p" => self.paragraph = Some(Block::default()),
            b"pPr" => self.in_paragraph_properties = true,
            b"r" => self.run = RunProps::default(),
            b"rPr" if !self.in_paragraph_properties => self.in_run_properties = true,
            b"b" if self.in_run_properties => {
                self.run.bold = on_off(attribute(reader, start, b"val")?.as_deref())?;
            }
            b"i" if self.in_run_properties => {
                self.run.italic = on_off(attribute(reader, start, b"val")?.as_deref())?;
            }
            b"strike" if self.in_run_properties => {
                self.run.strike = on_off(attribute(reader, start, b"val")?.as_deref())?;
            }
            b"u" if self.in_run_properties => {
                let value = attribute(reader, start, b"val")?.ok_or_else(|| {
                    DocxError::Structure("u without a val names no underline style".to_owned())
                })?;
                self.run.underline = (value != "none").then_some(value);
            }
            b"sz" if self.in_run_properties => {
                self.run.size = attribute(reader, start, b"val")?;
            }
            b"rFonts" if self.in_run_properties => {
                self.run.family = attribute(reader, start, b"ascii")?;
            }
            b"color" if self.in_run_properties => {
                self.run.color = attribute(reader, start, b"val")?;
            }
            b"t" => self.in_text = true,
            b"tab" if !self.in_paragraph_properties => self.append("\t"),
            b"br" | b"cr" => self.append("\n"),
            b"sym" if !self.in_paragraph_properties => {
                let value = attribute(reader, start, b"char")?.ok_or_else(|| {
                    DocxError::Structure("sym without a char attribute".to_owned())
                })?;
                self.append(&sym_char(&value)?.to_string());
            }
            b"noBreakHyphen" if !self.in_paragraph_properties => self.append("\u{2011}"),
            b"softHyphen" if !self.in_paragraph_properties => self.append("\u{00ad}"),
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
            b"pPr" => self.in_paragraph_properties = false,
            b"rPr" => self.in_run_properties = false,
            b"t" => self.in_text = false,
            b"p" => {
                let block = self.paragraph.take().ok_or_else(|| {
                    DocxError::Structure("paragraph end without a paragraph".to_owned())
                })?;
                self.blocks.push(block);
            }
            _ => {}
        }
        Ok(())
    }

    fn text(&mut self, content: &str) -> Result<(), DocxError> {
        if self.skipping > 0 || !self.in_text {
            return Ok(());
        }
        if self.paragraph.is_none() {
            return Err(DocxError::Structure("text outside a paragraph".to_owned()));
        }
        self.append(content);
        Ok(())
    }
}

impl BlockWalk {
    fn append(&mut self, content: &str) {
        if let Some(block) = self.paragraph.as_mut() {
            block.append(content, &self.run);
        }
    }
}

fn blocks(bytes: &[u8]) -> Result<Vec<Block>, DocxError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let (document, _) = document_part(&mut archive)?;
    let mut walk = BlockWalk::default();
    walk_body(&document, &mut walk)?;
    Ok(walk.blocks)
}

/// The rich deltas between two docx versions: one per maximal character
/// range whose run properties changed, on paragraphs whose text did not.
/// Paragraph numbers count the after side's accepted-body paragraphs in
/// document order, table cells included.
pub(crate) fn rich_deltas(
    package: PackageId,
    before: &[u8],
    after: &[u8],
) -> Result<Vec<Delta>, DocxError> {
    let before_blocks = blocks(before)?;
    let after_blocks = blocks(after)?;
    let before_texts: Vec<&str> = before_blocks.iter().map(|b| b.text.as_str()).collect();
    let after_texts: Vec<&str> = after_blocks.iter().map(|b| b.text.as_str()).collect();

    let mut deltas = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &before_texts, &after_texts) {
        let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        else {
            continue;
        };
        for offset in 0..len {
            let old = &before_blocks[old_index + offset];
            let new = &after_blocks[new_index + offset];
            for (text, before_props, after_props) in changed_ranges(old, new) {
                if let Some(described) = describe(&before_props, &after_props)? {
                    deltas.push(Delta::rich(
                        Address::new(format!("paragraph {}", new_index + offset + 1)),
                        format!("{text:?} {described}"),
                        package,
                    ));
                }
            }
        }
    }
    Ok(deltas)
}

/// Maximal character ranges of one equal-text paragraph pair whose
/// properties changed, each with its text and both property sets. Ranges
/// group by the (before, after) pair, so where the change itself changes a
/// new range starts — run boundaries never show through.
fn changed_ranges(old: &Block, new: &Block) -> Vec<(String, RunProps, RunProps)> {
    let mut ranges: Vec<(String, RunProps, RunProps)> = Vec::new();
    let mut open = false;
    for ((c, before), after) in new.text.chars().zip(old.char_props()).zip(new.char_props()) {
        if before == after {
            open = false;
            continue;
        }
        match ranges.last_mut() {
            Some((text, last_before, last_after))
                if open && *last_before == *before && *last_after == *after =>
            {
                text.push(c);
            }
            _ => {
                ranges.push((c.to_string(), before.clone(), after.clone()));
                open = true;
            }
        }
    }
    ranges
}

/// The change between two property sets, in the format's own terms —
/// `gained bold, font size 11 → 14`. `None` when only the emphasis trio
/// changed: the text rung already carries bold, italic, and strikethrough
/// as markdown markers, so alone they are not this rung's news.
fn describe(before: &RunProps, after: &RunProps) -> Result<Option<String>, DocxError> {
    let mut trio = Vec::new();
    toggle(&mut trio, "bold", before.bold, after.bold);
    toggle(&mut trio, "italic", before.italic, after.italic);
    toggle(&mut trio, "strikethrough", before.strike, after.strike);

    let mut clauses = Vec::new();
    if before.underline != after.underline {
        clauses.push(format!(
            "underline {} → {}",
            before.underline.as_deref().unwrap_or("none"),
            after.underline.as_deref().unwrap_or("none"),
        ));
    }
    if before.size != after.size {
        clauses.push(format!(
            "font size {} → {}",
            points(before.size.as_deref())?,
            points(after.size.as_deref())?,
        ));
    }
    if before.family != after.family {
        clauses.push(format!(
            "font {} → {}",
            before.family.as_deref().unwrap_or("default"),
            after.family.as_deref().unwrap_or("default"),
        ));
    }
    if before.color != after.color {
        clauses.push(format!(
            "color {} → {}",
            before.color.as_deref().unwrap_or("default"),
            after.color.as_deref().unwrap_or("default"),
        ));
    }
    if clauses.is_empty() {
        return Ok(None);
    }
    let mut all = trio;
    all.append(&mut clauses);
    Ok(Some(all.join(", ")))
}

fn toggle(clauses: &mut Vec<String>, name: &str, before: bool, after: bool) {
    match (before, after) {
        (false, true) => clauses.push(format!("gained {name}")),
        (true, false) => clauses.push(format!("lost {name}")),
        (true, true) | (false, false) => {}
    }
}

/// A `w:sz` half-point value as points: `22` → `11`, `23` → `11.5`. An
/// absent size is the document default.
fn points(size: Option<&str>) -> Result<String, DocxError> {
    let Some(size) = size else {
        return Ok("default".to_owned());
    };
    let halves: u64 = size
        .parse()
        .map_err(|_| DocxError::Structure(format!("font size {size:?} is not half-points")))?;
    if halves.is_multiple_of(2) {
        Ok((halves / 2).to_string())
    } else {
        Ok(format!("{}.5", halves / 2))
    }
}
