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
    WorkspaceInit,
    SourceAttach,
    Snapshot,
    /// A format package failed or panicked over a document; the diff fell
    /// back to the binary rung. The entry's reference names the document,
    /// the package, and the reason — degradation is never silent.
    PackageFailed,
    /// A file exceeded the ladder's size cap; its delta stayed at the
    /// binary rung. The entry's reference names the file and the cap.
    FileTooLarge,
}

impl Act {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceInit => "workspace_init",
            Self::SourceAttach => "source_attach",
            Self::Snapshot => "snapshot",
            Self::PackageFailed => "package_failed",
            Self::FileTooLarge => "file_too_large",
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
    pub at_ms: i64,
    pub actor_name: String,
    pub actor_kind: ActorKind,
    pub act: Act,
    pub session: Option<String>,
    pub instruction_summary: Option<String>,
    pub instruction_run_ref: Option<String>,
    pub instruction_verbatim: Option<String>,
    pub reference: Option<String>,
}

/// The append-only journal, a SQLite database beside the repo.
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Open (creating if absent) the journal at `path` and ensure its schema.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open(path).map_err(engine_err)?;
        let journal = Self { conn };
        journal.ensure_schema()?;
        Ok(journal)
    }

    fn ensure_schema(&self) -> Result<(), Error> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS journal (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    at_ms INTEGER NOT NULL,
                    actor_name TEXT NOT NULL,
                    actor_kind TEXT NOT NULL,
                    act TEXT NOT NULL,
                    session TEXT,
                    instruction_summary TEXT,
                    instruction_run_ref TEXT,
                    instruction_verbatim TEXT,
                    reference TEXT
                );",
            )
            .map_err(engine_err)?;
        self.conn
            .pragma_update(None, "user_version", 1)
            .map_err(engine_err)?;
        Ok(())
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
