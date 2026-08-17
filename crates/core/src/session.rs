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
    /// A one-line statement of what the session is for.
    pub summary: String,
    /// A reference to the run that carried the instruction: a ticket, a
    /// conversation, a pipeline id.
    pub run_ref: Option<String>,
    /// The instruction's verbatim body, when the caller supplies it.
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
    /// The session's identity.
    pub id: SessionId,
    /// Who the session belongs to.
    pub actor: Actor,
    /// Where the session stands.
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
    /// The instruction's summary, as journaled at open.
    pub instruction_summary: String,
    /// The instruction's run reference, when one was given.
    pub instruction_run_ref: Option<String>,
    /// When the session opened, in unix milliseconds.
    pub opened_at_ms: i64,
}

/// One source's change under a session: the root's when `source` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceChange {
    /// The mount the change belongs to; `None` for the root.
    pub source: Option<String>,
    /// The stable identity of the session's unit of work on this source.
    pub change_id: String,
}

/// Where a session stands: open for work, its change landed, or closed
/// without landing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Open for work.
    Open,
    /// The session's change landed on the shared line.
    Landed,
    /// Closed without landing; its work stays in history.
    Abandoned,
}

impl SessionState {
    /// The state's canonical lowercase name, as stored and rendered.
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
