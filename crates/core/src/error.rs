use std::path::PathBuf;

use thiserror::Error;

/// Every way a core operation can fail.
///
/// The domain variants are outcomes callers match on by name; the last
/// three wrap a lower layer (the engine, the filesystem, config parsing)
/// whose detail is carried as a message.
#[derive(Debug, Error)]
pub enum Error {
    #[error("not a workspace: {0}")]
    NotAWorkspace(PathBuf),

    #[error("a workspace already exists at {0}")]
    WorkspaceExists(PathBuf),

    #[error("cannot nest a workspace inside the one at {0}")]
    NestedWorkspace(PathBuf),

    #[error("a source is already attached to this workspace")]
    AlreadyAttached,

    #[error("git-lfs sources are unsupported")]
    LfsSourceUnsupported,

    #[error("no actor is configured")]
    NoActorConfigured,

    #[error("no session {0}")]
    SessionNotFound(String),

    #[error("session {id} is {state}")]
    SessionClosed { id: String, state: String },

    #[error("no landing request {0}")]
    RequestNotFound(String),

    #[error("landing request {id} is {state}")]
    RequestClosed { id: String, state: String },

    #[error(
        "landing request {0} is parked on a conflict; a new snapshot on its change re-opens the gate"
    )]
    RequestParked(String),

    #[error("this workspace forbids approving your own landing request")]
    SelfApprovalForbidden,

    #[error("approvals were dismissed: the change has a new snapshot {new_snapshot}")]
    ApprovalsDismissed { new_snapshot: String },

    #[error("the landing lease is held by {holder} until {expires_at_ms}")]
    LeaseHeld { holder: String, expires_at_ms: i64 },

    #[error("path {0} leaves the session working copy")]
    PathOutsideWorkingCopy(String),

    #[error("no format package projects {0} and it is not utf-8 text")]
    NotText(String),

    #[error("read windows span 1 to {max} bytes")]
    WindowTooLarge { max: usize },

    #[error("package {package} failed: {reason}")]
    PackageFailed { package: String, reason: String },

    #[error("engine error: {0}")]
    Engine(String),

    // The io source is deliberately absent from the message: callers render
    // the error chain, where the source already appears once.
    #[error("i/o error")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),
}

/// Wrap an engine-layer failure (jj, `SQLite`) as [`Error::Engine`].
pub(crate) fn engine_err(source: impl std::fmt::Display) -> Error {
    Error::Engine(source.to_string())
}

/// Wrap a config-layer failure (TOML, time) as [`Error::Config`].
pub(crate) fn config_err(source: impl std::fmt::Display) -> Error {
    Error::Config(source.to_string())
}
