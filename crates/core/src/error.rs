use std::path::PathBuf;

use thiserror::Error;

/// Every way a core operation can fail.
///
/// The domain variants are outcomes callers match on by name; the last
/// three wrap a lower layer (the engine, the filesystem, config parsing)
/// whose detail is carried as a message.
#[derive(Debug, Error)]
pub enum Error {
    /// The path is not inside an atelier workspace.
    #[error("not a workspace: {0}")]
    NotAWorkspace(PathBuf),

    /// `init` refused: the path already holds a workspace.
    #[error("a workspace already exists at {0}")]
    WorkspaceExists(PathBuf),

    /// `init` refused: the path sits inside an existing workspace.
    #[error("cannot nest a workspace inside the one at {0}")]
    NestedWorkspace(PathBuf),

    /// `init` refused: the path already holds a git repository.
    #[error(
        "cannot initialize a workspace at {0}: it is already a git repository; initialize a workspace elsewhere, then run: atelier attach {0} --mount <name>"
    )]
    GitRepositoryExists(PathBuf),

    /// A colocated git repo moved out of band and the moved content
    /// conflicts with the line; the fold refused — the shared line never
    /// carries a conflicted state.
    #[error(
        "the git branch {branch:?} moved out of band and its changes conflict with the line; make the working copy agree with the branch (or move the branch), then retry"
    )]
    GitFoldConflicted {
        /// The branch whose out-of-band move conflicts.
        branch: String,
    },

    /// `attach` refused: the folder is a linked git worktree whose
    /// state its repository owns; only the repository attaches whole.
    #[error(
        "{folder} is a linked git worktree of {repository}; linked worktrees must be cloned before attachment: git clone --no-local --single-branch -- <worktree> <new-source>, then atelier attach <new-source> --mount <name>; cloning copies the worktree's committed HEAD, not uncommitted edits"
    )]
    LinkedWorktreeUnsupported {
        /// The linked worktree that was offered as a source.
        folder: PathBuf,
        /// The repository the worktree belongs to.
        repository: PathBuf,
    },

    /// `attach` refused: the mount name is already taken.
    #[error("a source is already attached to this workspace")]
    AlreadyAttached,

    /// Adoption refused: the git source uses git-lfs.
    #[error("git-lfs sources are unsupported")]
    LfsSourceUnsupported,

    /// No config home names an actor; see `resolve_actor`.
    #[error("no actor is configured")]
    NoActorConfigured,

    /// No session carries the given id.
    #[error("no session {0}")]
    SessionNotFound(String),

    /// The session left the open state; the operation needs it open.
    #[error("session {id} is {state}")]
    SessionClosed {
        /// The session's id.
        id: String,
        /// The state the session is in.
        state: String,
    },

    /// No landing request carries the given id.
    #[error("no landing request {0}")]
    RequestNotFound(String),

    /// The landing request left the open state; the operation needs it open.
    #[error("landing request {id} is {state}")]
    RequestClosed {
        /// The request's id.
        id: String,
        /// The state the request is in.
        state: String,
    },

    /// The landing request hit a conflict and parked.
    #[error(
        "landing request {0} is parked on a conflict; a new snapshot on its change re-opens the gate"
    )]
    RequestParked(String),

    /// The workspace's landing policy forbids self-approval.
    #[error("this workspace forbids approving your own landing request")]
    SelfApprovalForbidden,

    /// The change gained a snapshot after approval, so the approvals no
    /// longer vouch for what would land.
    #[error("approvals were dismissed: the change has a new snapshot {new_snapshot}")]
    ApprovalsDismissed {
        /// The snapshot that dismissed the approvals.
        new_snapshot: String,
    },

    /// A leased line move lost its tenancy before publishing: a newer
    /// claim superseded the lease while this holder worked. Nothing
    /// moved.
    #[error(
        "the landing lease for {point:?} was superseded before anything published; rerun the operation"
    )]
    LeaseSuperseded {
        /// The landing point whose lease was superseded.
        point: String,
    },

    /// Another actor holds the landing lease.
    #[error("the landing lease is held by {holder} until {expires_at_ms}")]
    LeaseHeld {
        /// The actor holding the lease.
        holder: String,
        /// When the lease expires, in unix milliseconds.
        expires_at_ms: i64,
    },

    /// The path escapes the session's working copy.
    #[error("path {0} leaves the session working copy")]
    PathOutsideWorkingCopy(String),

    /// The file has no projecting package and its bytes are not text.
    #[error("no format package projects {0} and it is not utf-8 text")]
    NotText(String),

    /// The requested read window exceeds the maximum.
    #[error("read windows span 1 to {max} bytes")]
    WindowTooLarge {
        /// The largest window a read accepts, in bytes.
        max: usize,
    },

    /// A format package failed while handling a document it claimed.
    #[error("package {package} failed: {reason}")]
    PackageFailed {
        /// The failing package's id.
        package: String,
        /// Why it failed, in the package's own words.
        reason: String,
    },

    /// An engine-layer failure: jj or `SQLite`.
    #[error("engine error: {0}")]
    Engine(String),

    /// A filesystem failure.
    // The io source is deliberately absent from the message: callers render
    // the error chain, where the source already appears once.
    #[error("i/o error")]
    Io(#[from] std::io::Error),

    /// A config-layer failure: TOML or time formatting.
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
