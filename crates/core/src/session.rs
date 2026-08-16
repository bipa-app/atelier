use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use crate::config::Actor;
use crate::error::Error;

/// The task or prompt that drove a session's acts. The journal keeps the
/// summary and run reference; verbatim capture is policy-decided (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub summary: String,
    pub run_ref: Option<String>,
    pub verbatim: Option<String>,
}

/// A session's identity: `s` plus its row in the workspace store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(pub(crate) i64);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let not_found = || Error::SessionNotFound(text.to_owned());
        let digits = text.strip_prefix('s').ok_or_else(not_found)?;
        let row: i64 = digits.parse().map_err(|_| not_found())?;
        Ok(Self(row))
    }
}

/// One actor's bounded run of work in a workspace: its own working copy,
/// its own change, journal entries grouped under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub actor: Actor,
    pub state: SessionState,
    /// The stable identity of the session's unit of work on the root —
    /// source zero; it survives rewrites — amended snapshots and the
    /// landing rebase.
    pub change_id: String,
    /// One change per source the session spans, root first then mounts in
    /// name order (ADR-0009).
    pub changes: Vec<SourceChange>,
    /// The session's editable directory, absolute, mirroring the
    /// workspace's shape: root files at its top, each mounted source's
    /// working copy at `<mount>/`. A real directory on disk: sessions
    /// survive process restarts.
    pub working_copy: PathBuf,
    pub instruction_summary: String,
    pub instruction_run_ref: Option<String>,
    pub opened_at_ms: i64,
}

/// One source's change under a session: the root's when `source` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceChange {
    pub source: Option<String>,
    pub change_id: String,
}

/// Where a session stands: open for work, its change landed, or closed
/// without landing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Open,
    Landed,
    Abandoned,
}

impl SessionState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Landed => "landed",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionState {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "open" => Ok(Self::Open),
            "landed" => Ok(Self::Landed),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(Error::Engine(format!("unknown session state: {other}"))),
        }
    }
}

impl ToSql for SessionState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for SessionState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::from_str(text).map_err(|error| FromSqlError::Other(error.to_string().into()))
    }
}
