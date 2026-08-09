//! The format-independent diff model and fidelity ladder.
//!
//! A [`Diff`] is a set of addressed [`Delta`]s, each carried at a
//! [`Fidelity`] rung. The binary rung here is the floor every document
//! diffs at; [`FormatPackage`]s project documents to text so the ladder can
//! raise deltas to the text rung, and later to rich deltas in the format's
//! own terms.

mod binary;
mod model;
mod package;
mod text;

pub use binary::diff_listings;
pub use model::{Address, Delta, DeltaKind, Diff, Fidelity, Line, LineKind};
pub use package::{Confidence, FormatPackage, PackageError, PackageId, Projection, detect_package};
pub use text::{NO_NEWLINE_MARKER, as_text, diff_lines};
