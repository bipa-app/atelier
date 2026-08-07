//! The atelier SDK and engine: the one core every face is a thin shell over.

mod config;
mod engine;
mod error;
mod journal;
mod workspace;

pub use error::Error;
pub use journal::JournalEntry;
pub use workspace::{Snapshot, Source, Workspace};
