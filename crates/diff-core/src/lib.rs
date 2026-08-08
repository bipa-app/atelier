//! The format-independent diff model and fidelity ladder.
//!
//! A [`Diff`] is a set of addressed [`Delta`]s carried at a [`Fidelity`].
//! Format packages raise fidelity; the binary rung here is the floor every
//! document diffs at, whatever its format.

mod binary;
mod model;

pub use binary::diff_listings;
pub use model::{Address, Delta, DeltaKind, Diff, Fidelity};
