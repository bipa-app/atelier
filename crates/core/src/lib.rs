//! The atelier SDK and engine: the one core every face is a thin shell over.

mod config;
mod engine;
mod error;
mod journal;
mod workspace;

pub use atelier_diff_core::{Address, Delta, DeltaKind, Diff, Fidelity};
pub use error::Error;
pub use journal::JournalEntry;
pub use workspace::{Snapshot, Source, Workspace};
