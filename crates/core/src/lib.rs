//! The atelier SDK and engine: versioned workspaces humans and AI agents
//! do real work in, with snapshots, isolated sessions, gated landing, and
//! a journal that answers who did what and why.
//!
//! A [`Workspace`] is a directory whose whole history atelier keeps: every
//! outstanding edit becomes an attributed [`Snapshot`] before any read
//! model answers. An actor works in a [`Session`] — its own working copy,
//! its own change — and lands through the gate: a [`LandingRequest`]
//! gathers approvals under the workspace's [`LandingPolicy`], then the
//! apply lands the change on the shared line or parks on a conflict —
//! never half-applies. Every act becomes a [`JournalEntry`] in the
//! append-only journal. Diffs ride a fidelity ladder ([`Diff`]): every
//! file compares at least as bytes, text raises to line diffs, and format
//! packages — Word documents via `atelier-format-docx` — raise to deltas
//! in the format's own terms. A workspace attaches sources — local
//! folders, git repositories, bucket prefixes — each mounted with its own
//! history; landings fan out per source and mirror back.
//!
//! The CLI and the MCP and HTTP surfaces are thin shells over this crate:
//! anything they do, the SDK does directly.
//!
//! # Example
//!
//! The actor comes from config (`$ATELIER_CONFIG_HOME/config.toml`, else
//! `$XDG_CONFIG_HOME/atelier/config.toml`, else
//! `~/.config/atelier/config.toml`):
//!
//! ```
//! use atelier_core::{GateOutcome, Instruction, Workspace};
//!
//! # #[expect(unsafe_code, reason = "set_var points the lookup at the scratch config")]
//! # fn set_config_home(home: &std::path::Path) {
//! #     // SAFETY: the doctest runs on this process's only thread.
//! #     unsafe { std::env::set_var("ATELIER_CONFIG_HOME", home) };
//! # }
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config = tempfile::tempdir()?;
//! std::fs::write(
//!     config.path().join("config.toml"),
//!     "[actor]\nname = \"ada\"\nkind = \"human\"\n",
//! )?;
//! # set_config_home(config.path());
//!
//! // A workspace, a session, one write, and a landing through the gate.
//! let root = tempfile::tempdir()?;
//! let mut workspace = Workspace::init(root.path())?;
//! let actor = workspace.actor().clone();
//! let session = workspace.open_session(
//!     &actor,
//!     &Instruction {
//!         summary: "draft the notes".to_owned(),
//!         run_ref: None,
//!         verbatim: None,
//!     },
//! )?;
//! workspace.session_write(session.id, "notes.md", "The first note.\n")?;
//! let outcome = workspace.land(session.id)?;
//! assert!(matches!(outcome, GateOutcome::Landed { .. }));
//! # Ok(())
//! # }
//! ```

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
pub use atelier_source_remote::is_remote_url;
pub use config::{
    Actor, ActorKind, InstructionFidelity, JournalPolicy, LandingPolicy, Source, SourceKind,
    SyncPolicy,
};
pub use error::Error;
pub use journal::{Act, JournalEntry};
pub use landing::{
    Approval, GateOutcome, Landing, LandingRequest, RequestId, RequestState, Restore,
};
pub use read::{READ_WINDOW_MAX, ReadResult, ReadWindow};
pub use render::{printable, render_diff};
pub use session::{Instruction, Session, SessionId, SessionState, SourceChange};
pub use watch::{WatchEvent, WatchStop};
pub use workspace::{PullOutcome, Snapshot, SourceSnapshot, SyncOutcome, Workspace};
