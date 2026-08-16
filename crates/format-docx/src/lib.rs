//! The docx format package: Word documents projected for atelier's ladder.
//!
//! Projector only in v1: the accepted body — paragraphs, headings, lists,
//! tables — renders to deterministic markdown. Tracked changes resolve to
//! the accepted body: pending deletions and pre-revision properties are
//! excluded, pending insertions included. Run emphasis a document names
//! directly — bold, italic, strikethrough — projects as markdown emphasis;
//! formatting markdown cannot express (font size, family, color,
//! underline) and styles-applied emphasis are not projected, nor are
//! comments. With no differ yet, the ladder diffs docx documents at the
//! text rung over these projections.

mod projection;

use std::path::Path;

use atelier_diff_core::{Confidence, FormatPackage, PackageError, PackageId, Projection};

/// Every zip archive — and so every docx — starts with these bytes.
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];

/// The docx package: projects Word documents to markdown.
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
}

fn has_docx_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("docx"))
}
