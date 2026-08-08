//! The atelier SDK and engine: the one core every face is a thin shell over.

mod config;
mod engine;
mod error;
mod journal;
mod workspace;

pub use atelier_diff_core::{Address, Delta, DeltaKind, Diff, Fidelity};
pub use config::{Actor, ActorKind, Source, SourceKind, SyncPolicy};
pub use error::Error;
pub use journal::{Act, JournalEntry};
pub use workspace::{Snapshot, Workspace};
