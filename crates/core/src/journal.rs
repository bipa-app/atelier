use std::fmt;
use std::path::Path;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, params};

use crate::config::ActorKind;
use crate::error::{Error, engine_err};

/// One thing an actor can do to a workspace that the journal records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// A workspace came into being.
    WorkspaceInit,
    /// A source was attached to the workspace; the reference names it.
    SourceAttach,
    /// A snapshot recorded the working copy; the reference names it.
    Snapshot,
    /// A format package failed or panicked over a document; the diff fell
    /// back to the binary rung. The entry's reference names the document,
    /// the package, and the reason — degradation is never silent.
    PackageFailed,
    /// A file exceeded the ladder's size cap; its delta stayed at the
    /// binary rung. The entry's reference names the file and the cap.
    FileTooLarge,
    /// An actor opened a session; the entry carries the instruction's
    /// summary and run reference (verbatim per policy, ADR-0004).
    SessionOpen,
    /// The session closed without landing; its work stays in history.
    SessionAbandon,
    /// A session opened a landing request; the reference names it.
    LandRequest,
    /// An actor approved a landing request; the reference names the
    /// request and the snapshot the approval covers.
    Approve,
    /// An actor rejected a landing request; the reference names it and
    /// carries the reason when one was given.
    Reject,
    /// A new snapshot on the change dismissed the request's approvals; the
    /// reference names the request and the snapshot.
    ApprovalsDismissed,
    /// A change landed on the shared line; the reference names the request
    /// and the landed snapshot.
    Land,
    /// A landing attempt hit a conflict and parked its request; the shared
    /// line did not move. The reference names the request.
    LandParked,
    /// A landed line mirrored back to its folder source; the reference
    /// names the source and the synced snapshot (ADR-0010).
    Sync,
    /// Bucket-side changes folded into a mounted line as one snapshot
    /// (ADR-0012, the pull); the reference names the source and snapshot.
    Pull,
    /// A landed request stepped back off a line: the head returned to the
    /// landed snapshot's parent, which the reference names with the
    /// request (ADR-0011).
    Undo,
    /// A sync could not run — the origin changed out-of-band or refused
    /// writes — and the landing stood anyway; the reference names the
    /// source and snapshot. `atelier sync` retries; never silent.
    SyncParked,
}

impl Act {
    /// The act's canonical `snake_case` name, as stored and rendered.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceInit => "workspace_init",
            Self::SourceAttach => "source_attach",
            Self::Snapshot => "snapshot",
            Self::PackageFailed => "package_failed",
            Self::FileTooLarge => "file_too_large",
            Self::SessionOpen => "session_open",
            Self::SessionAbandon => "session_abandon",
            Self::LandRequest => "land_request",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::ApprovalsDismissed => "approvals_dismissed",
            Self::Land => "land",
            Self::LandParked => "land_parked",
            Self::Pull => "pull",
            Self::Undo => "undo",
            Self::Sync => "sync",
            Self::SyncParked => "sync_parked",
        }
    }
}

impl fmt::Display for Act {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Act {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "workspace_init" => Ok(Self::WorkspaceInit),
            "source_attach" => Ok(Self::SourceAttach),
            "snapshot" => Ok(Self::Snapshot),
            "package_failed" => Ok(Self::PackageFailed),
            "file_too_large" => Ok(Self::FileTooLarge),
            "session_open" => Ok(Self::SessionOpen),
            "session_abandon" => Ok(Self::SessionAbandon),
            "land_request" => Ok(Self::LandRequest),
            "approve" => Ok(Self::Approve),
            "reject" => Ok(Self::Reject),
            "approvals_dismissed" => Ok(Self::ApprovalsDismissed),
            "land" => Ok(Self::Land),
            "land_parked" => Ok(Self::LandParked),
            "pull" => Ok(Self::Pull),
            "undo" => Ok(Self::Undo),
            "sync" => Ok(Self::Sync),
            "sync_parked" => Ok(Self::SyncParked),
            other => Err(Error::Engine(format!("unknown journal act: {other}"))),
        }
    }
}

impl ToSql for Act {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Act {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::from_str(text).map_err(|error| FromSqlError::Other(error.to_string().into()))
    }
}

impl ToSql for ActorKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for ActorKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::from_str(text).map_err(|error| FromSqlError::Other(error.to_string().into()))
    }
}

/// One record in a workspace's journal: who did what, and any intent behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// When the act happened, in unix milliseconds.
    pub at_ms: i64,
    /// The acting actor's display name.
    pub actor_name: String,
    /// What kind of actor acted.
    pub actor_kind: ActorKind,
    /// What the actor did.
    pub act: Act,
    /// The session the act belongs to, when it happened inside one.
    pub session: Option<String>,
    /// The instruction's summary, on `session_open` entries.
    pub instruction_summary: Option<String>,
    /// A reference to the run that carried the instruction.
    pub instruction_run_ref: Option<String>,
    /// The instruction's verbatim body, when policy keeps it (ADR-0004).
    pub instruction_verbatim: Option<String>,
    /// What the act refers to, in the act's own terms: a snapshot, a
    /// request, a source.
    pub reference: Option<String>,
}

/// The append-only journal, a `SQLite` database beside the repo.
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Open (creating if absent) the journal at `path` and ensure its schema.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = crate::store::open_connection(path)?;
        Ok(Self { conn })
    }

    /// Append one entry to the journal.
    pub fn append(&self, entry: &JournalEntry) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT INTO journal (
                    at_ms, actor_name, actor_kind, act, session,
                    instruction_summary, instruction_run_ref, instruction_verbatim, reference
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entry.at_ms,
                    entry.actor_name,
                    entry.actor_kind,
                    entry.act,
                    entry.session,
                    entry.instruction_summary,
                    entry.instruction_run_ref,
                    entry.instruction_verbatim,
                    entry.reference,
                ],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    /// Read up to `limit` entries, newest first.
    pub fn entries(&self, limit: usize) -> Result<Vec<JournalEntry>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT at_ms, actor_name, actor_kind, act, session,
                        instruction_summary, instruction_run_ref, instruction_verbatim, reference
                 FROM journal ORDER BY id DESC LIMIT ?1",
            )
            .map_err(engine_err)?;
        let limit = i64::try_from(limit).map_err(engine_err)?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(JournalEntry {
                    at_ms: row.get(0)?,
                    actor_name: row.get(1)?,
                    actor_kind: row.get(2)?,
                    act: row.get(3)?,
                    session: row.get(4)?,
                    instruction_summary: row.get(5)?,
                    instruction_run_ref: row.get(6)?,
                    instruction_verbatim: row.get(7)?,
                    reference: row.get(8)?,
                })
            })
            .map_err(engine_err)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row.map_err(engine_err)?);
        }
        Ok(entries)
    }
}
