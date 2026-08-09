use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::config::{Actor, ActorKind};
use crate::error::{Error, engine_err};
use crate::landing::RequestState;
use crate::session::SessionState;

/// The coordination port's local implementation (ADR-0008): sessions,
/// landing requests, approvals, and the landing lease as rows in the
/// workspace's `SQLite` database. One statement per claim — atomic and
/// correct across the CLI and a server sharing the workspace.
pub(crate) struct Coordination {
    conn: Connection,
}

pub(crate) struct SessionRow {
    pub id: i64,
    pub actor_name: String,
    pub actor_kind: ActorKind,
    pub instruction_summary: String,
    pub instruction_run_ref: Option<String>,
    pub change_id: Option<String>,
    pub state: SessionState,
    pub opened_at_ms: i64,
}

pub(crate) struct RequestRow {
    pub id: i64,
    pub session_id: i64,
    pub requester_name: String,
    pub requester_kind: ActorKind,
    pub state: RequestState,
    pub created_at_ms: i64,
}

pub(crate) struct ApprovalRow {
    pub actor_name: String,
    pub actor_kind: ActorKind,
    pub snapshot_id: String,
    pub at_ms: i64,
}

/// What one attempt to claim a lease produced.
pub(crate) enum LeaseClaim {
    Held,
    HeldByOther { holder: String, expires_at_ms: i64 },
}

impl Coordination {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = crate::store::open_connection(path)?;
        Ok(Self { conn })
    }

    pub fn create_session(
        &self,
        actor: &Actor,
        summary: &str,
        run_ref: Option<&str>,
        verbatim: Option<&str>,
        at_ms: i64,
    ) -> Result<i64, Error> {
        self.conn
            .execute(
                "INSERT INTO sessions (
                    actor_name, actor_kind, instruction_summary, instruction_run_ref,
                    instruction_verbatim, state, opened_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    actor.name,
                    actor.kind,
                    summary,
                    run_ref,
                    verbatim,
                    SessionState::Open,
                    at_ms,
                ],
            )
            .map_err(engine_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Remove a session whose engine bootstrap failed: its row never
    /// carried a change, so nothing refers to it.
    pub fn delete_session(&self, id: i64) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(engine_err)?;
        Ok(())
    }

    pub fn set_session_change(&self, id: i64, change_id: &str) -> Result<(), Error> {
        self.conn
            .execute(
                "UPDATE sessions SET change_id = ?2 WHERE id = ?1",
                params![id, change_id],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    pub fn set_session_state(&self, id: i64, state: SessionState) -> Result<(), Error> {
        self.conn
            .execute(
                "UPDATE sessions SET state = ?2 WHERE id = ?1",
                params![id, state],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    pub fn session(&self, id: i64) -> Result<Option<SessionRow>, Error> {
        self.conn
            .query_row(
                "SELECT id, actor_name, actor_kind, instruction_summary, instruction_run_ref,
                        change_id, state, opened_at_ms
                 FROM sessions WHERE id = ?1",
                params![id],
                session_row,
            )
            .optional()
            .map_err(engine_err)
    }

    /// Every session, newest first.
    pub fn sessions(&self) -> Result<Vec<SessionRow>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, actor_name, actor_kind, instruction_summary, instruction_run_ref,
                        change_id, state, opened_at_ms
                 FROM sessions ORDER BY id DESC",
            )
            .map_err(engine_err)?;
        let rows = stmt.query_map([], session_row).map_err(engine_err)?;
        collect(rows)
    }

    pub fn create_request(
        &self,
        session_id: i64,
        requester: &Actor,
        at_ms: i64,
    ) -> Result<i64, Error> {
        self.conn
            .execute(
                "INSERT INTO landing_requests (
                    session_id, requester_name, requester_kind, state, created_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    requester.name,
                    requester.kind,
                    RequestState::Open,
                    at_ms,
                ],
            )
            .map_err(engine_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn request(&self, id: i64) -> Result<Option<RequestRow>, Error> {
        self.conn
            .query_row(
                "SELECT id, session_id, requester_name, requester_kind, state, created_at_ms
                 FROM landing_requests WHERE id = ?1",
                params![id],
                request_row,
            )
            .optional()
            .map_err(engine_err)
    }

    /// Every landing request, newest first.
    pub fn requests(&self) -> Result<Vec<RequestRow>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, requester_name, requester_kind, state, created_at_ms
                 FROM landing_requests ORDER BY id DESC",
            )
            .map_err(engine_err)?;
        let rows = stmt.query_map([], request_row).map_err(engine_err)?;
        collect(rows)
    }

    /// The session's request still holding a claim on the gate: open,
    /// approved, or parked. At most one exists per session.
    pub fn gated_request_for_session(&self, session_id: i64) -> Result<Option<RequestRow>, Error> {
        self.conn
            .query_row(
                "SELECT id, session_id, requester_name, requester_kind, state, created_at_ms
                 FROM landing_requests
                 WHERE session_id = ?1 AND state IN ('open', 'approved', 'parked')
                 ORDER BY id DESC LIMIT 1",
                params![session_id],
                request_row,
            )
            .optional()
            .map_err(engine_err)
    }

    pub fn set_request_state(&self, id: i64, state: RequestState) -> Result<(), Error> {
        self.conn
            .execute(
                "UPDATE landing_requests SET state = ?2 WHERE id = ?1",
                params![id, state],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    pub fn add_approval(
        &self,
        request_id: i64,
        actor: &Actor,
        snapshot_id: &str,
        at_ms: i64,
    ) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT INTO approvals (request_id, actor_name, actor_kind, snapshot_id, at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![request_id, actor.name, actor.kind, snapshot_id, at_ms],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    /// The approvals still counting toward the request's gate, oldest first.
    pub fn live_approvals(&self, request_id: i64) -> Result<Vec<ApprovalRow>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT actor_name, actor_kind, snapshot_id, at_ms
                 FROM approvals WHERE request_id = ?1 AND dismissed = 0 ORDER BY id",
            )
            .map_err(engine_err)?;
        let rows = stmt
            .query_map(params![request_id], |row| {
                Ok(ApprovalRow {
                    actor_name: row.get(0)?,
                    actor_kind: row.get(1)?,
                    snapshot_id: row.get(2)?,
                    at_ms: row.get(3)?,
                })
            })
            .map_err(engine_err)?;
        collect(rows)
    }

    /// Dismiss the request's live approvals; how many were dismissed.
    pub fn dismiss_approvals(&self, request_id: i64) -> Result<usize, Error> {
        self.conn
            .execute(
                "UPDATE approvals SET dismissed = 1 WHERE request_id = ?1 AND dismissed = 0",
                params![request_id],
            )
            .map_err(engine_err)
    }

    /// Claim `point` for `holder` until `now_ms + ttl_ms`. One statement,
    /// so exactly one claimant wins whatever processes race: the claim
    /// succeeds when the point is free, expired, or already this holder's.
    pub fn claim_lease(
        &self,
        point: &str,
        holder: &str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<LeaseClaim, Error> {
        loop {
            let claimed = self
                .conn
                .execute(
                    "INSERT INTO lease (point, holder, expires_at_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT(point) DO UPDATE
                     SET holder = excluded.holder, expires_at_ms = excluded.expires_at_ms
                     WHERE lease.expires_at_ms <= ?4 OR lease.holder = excluded.holder",
                    params![point, holder, now_ms + ttl_ms, now_ms],
                )
                .map_err(engine_err)?;
            if claimed == 1 {
                return Ok(LeaseClaim::Held);
            }
            // The holder may release between the failed claim and this
            // read; an absent row just means the point freed — claim again.
            let held = self
                .conn
                .query_row(
                    "SELECT holder, expires_at_ms FROM lease WHERE point = ?1",
                    params![point],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(engine_err)?;
            if let Some((holder, expires_at_ms)) = held {
                return Ok(LeaseClaim::HeldByOther {
                    holder,
                    expires_at_ms,
                });
            }
        }
    }

    /// Release `point` when this holder still owns it; an expired-and-taken
    /// lease belongs to its new holder and stays.
    pub fn release_lease(&self, point: &str, holder: &str) -> Result<(), Error> {
        self.conn
            .execute(
                "DELETE FROM lease WHERE point = ?1 AND holder = ?2",
                params![point, holder],
            )
            .map_err(engine_err)?;
        Ok(())
    }
}

fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        actor_name: row.get(1)?,
        actor_kind: row.get(2)?,
        instruction_summary: row.get(3)?,
        instruction_run_ref: row.get(4)?,
        change_id: row.get(5)?,
        state: row.get(6)?,
        opened_at_ms: row.get(7)?,
    })
}

fn request_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        requester_name: row.get(2)?,
        requester_kind: row.get(3)?,
        state: row.get(4)?,
        created_at_ms: row.get(5)?,
    })
}

fn collect<T>(rows: impl Iterator<Item = Result<T, rusqlite::Error>>) -> Result<Vec<T>, Error> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(engine_err)?);
    }
    Ok(out)
}
