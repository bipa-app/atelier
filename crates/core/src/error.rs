use std::path::PathBuf;

use thiserror::Error;

/// Every way a core operation can fail.
///
/// The first six variants are domain outcomes callers match on by name; the
/// last three wrap a lower layer (the engine, the filesystem, config parsing)
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

    #[error("engine error: {0}")]
    Engine(String),

    // The io source is deliberately absent from the message: callers render
    // the error chain, where the source already appears once.
    #[error("i/o error")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),
}

/// Wrap an engine-layer failure (jj, SQLite) as [`Error::Engine`].
pub(crate) fn engine_err(source: impl std::fmt::Display) -> Error {
    Error::Engine(source.to_string())
}

/// Wrap a config-layer failure (TOML, time) as [`Error::Config`].
pub(crate) fn config_err(source: impl std::fmt::Display) -> Error {
    Error::Config(source.to_string())
}
