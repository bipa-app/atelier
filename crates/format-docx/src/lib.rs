//! The docx format package: Word documents projected and diffed for
//! atelier's ladder.
//!
//! The projector renders the accepted body — paragraphs, headings, lists,
//! tables — to deterministic markdown. Tracked changes resolve to the
//! accepted body: pending deletions and pre-revision properties are
//! excluded, pending insertions included. Run emphasis a document names
//! directly — bold, italic, strikethrough — projects as markdown emphasis;
//! comments and styles-applied formatting are not projected.
//!
//! The differ carries the Rich rung additively: deltas for run formatting
//! markdown cannot express — font size, family, color, underline, and the
//! emphasis trio when it co-occurs with one of those — on paragraphs whose
//! text is unchanged. Text changes stay the text rung's story.

mod differ;
mod projection;

use std::path::Path;

use atelier_diff_core::{Confidence, Delta, FormatPackage, PackageError, PackageId, Projection};

/// Every zip archive — and so every docx — starts with these bytes.
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];

/// The docx package: projects Word documents to markdown and diffs their
/// run formatting at the Rich rung.
#[derive(Debug, Clone, Copy, Default)]
pub struct DocxPackage;

impl FormatPackage for DocxPackage {
    fn id(&self) -> PackageId {
        PackageId {
            name: "format-docx",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn detect(&self, path: &str, bytes: &[u8]) -> Option<Confidence> {
        if !has_docx_extension(path) {
            return None;
        }
        if bytes.starts_with(&ZIP_MAGIC) {
            return Some(Confidence::Content);
        }
        // A .docx that is not a zip — truncated, or encrypted inside an OLE
        // container — is still this package's document: claiming it makes
        // the projection failure loud (journaled fallback) instead of
        // letting the ladder misread the bytes as plain text.
        Some(Confidence::Extension)
    }

    fn project(&self, bytes: &[u8]) -> Result<Projection, PackageError> {
        match projection::markdown(bytes) {
            Ok(text) => Ok(Projection {
                package: self.id(),
                text,
            }),
            Err(error) => Err(PackageError {
                package: self.id(),
                reason: error.to_string(),
            }),
        }
    }

    fn diff(&self, before: &[u8], after: &[u8]) -> Option<Result<Vec<Delta>, PackageError>> {
        Some(
            differ::rich_deltas(self.id(), before, after).map_err(|error| PackageError {
                package: self.id(),
                reason: error.to_string(),
            }),
        )
    }
}

fn has_docx_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("docx"))
}
