//! The atelier SDK and engine: the one core every face is a thin shell over.

mod config;
mod coordination;
mod engine;
mod error;
mod journal;
mod landing;
mod projection;
mod read;
mod render;
mod session;
mod store;
mod watch;
mod workspace;

pub use atelier_diff_core::{
    Address, Confidence, Delta, DeltaKind, Diff, Fidelity, Line, LineKind, PackageId,
};
pub use config::{
    Actor, ActorKind, InstructionFidelity, JournalPolicy, LandingPolicy, Source, SourceKind,
    SyncPolicy,
};
pub use error::Error;
pub use journal::{Act, JournalEntry};
pub use landing::{Approval, GateOutcome, LandingRequest, RequestId, RequestState};
pub use read::{MAX_READ_WINDOW, ReadResult, ReadWindow};
pub use render::{printable, render_diff};
pub use session::{Instruction, Session, SessionId, SessionState};
pub use watch::{WatchEvent, WatchStop};
pub use workspace::{Snapshot, Workspace};
