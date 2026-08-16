use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::config::{Actor, ActorKind, ROOT_MOUNT};
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

    /// Record a mounted source's change under the session; the root's
    /// change lives on the session row itself.
    pub fn set_session_source_change(
        &self,
        session_id: i64,
        source: &str,
        change_id: &str,
    ) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT INTO session_changes (session_id, source, change_id)
                 VALUES (?1, ?2, ?3)",
                params![session_id, source, change_id],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    /// The session's mounted-source changes, in source order.
    pub fn session_source_changes(&self, session_id: i64) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, change_id FROM session_changes
                 WHERE session_id = ?1 ORDER BY source",
            )
            .map_err(engine_err)?;
        let rows = stmt
            .query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(engine_err)?;
        collect(rows)
    }

    /// Move the session's state when it still holds `from`; whether the
    /// row moved. A stale writer — racing another process past the same
    /// check — loses here instead of overwriting.
    pub fn move_session_state(
        &self,
        id: i64,
        from: SessionState,
        to: SessionState,
    ) -> Result<bool, Error> {
        let moved = self
            .conn
            .execute(
                "UPDATE sessions SET state = ?3 WHERE id = ?1 AND state = ?2",
                params![id, from, to],
            )
            .map_err(engine_err)?;
        Ok(moved == 1)
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

    /// Move the request's state when it still holds one of `from`;
    /// whether the row moved. The predicate names the expected prior
    /// states, so a write racing another process fails instead of
    /// overwriting the winner's transition.
    pub fn move_request_state(
        &self,
        id: i64,
        from: &[RequestState],
        to: RequestState,
    ) -> Result<bool, Error> {
        let placeholders: Vec<String> = (0..from.len()).map(|i| format!("?{}", i + 3)).collect();
        let sql = format!(
            "UPDATE landing_requests SET state = ?2 WHERE id = ?1 AND state IN ({})",
            placeholders.join(", ")
        );
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&id, &to];
        for state in from {
            values.push(state);
        }
        let moved = self.conn.execute(&sql, &values[..]).map_err(engine_err)?;
        Ok(moved == 1)
    }

    /// The origin fingerprint a source last synced at, if it ever has.
    pub fn sync_state(&self, mount: &str) -> Result<Option<String>, Error> {
        self.conn
            .query_row(
                "SELECT fingerprint FROM sync_state WHERE mount = ?1",
                params![mount],
                |row| row.get(0),
            )
            .optional()
            .map_err(engine_err)
    }

    /// Record a completed sync: the origin now mirrors `snapshot_id`, and
    /// its content hashes to `fingerprint`.
    pub fn record_sync_state(
        &self,
        mount: &str,
        fingerprint: &str,
        snapshot_id: &str,
    ) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT INTO sync_state (mount, fingerprint, snapshot_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(mount) DO UPDATE
                 SET fingerprint = excluded.fingerprint,
                     snapshot_id = excluded.snapshot_id",
                params![mount, fingerprint, snapshot_id],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    /// Record one source's landing under the request — the fact a re-apply
    /// after a park must not repeat. The root records as `/`.
    pub fn record_landing(
        &self,
        request_id: i64,
        source: Option<&str>,
        snapshot_id: &str,
    ) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT INTO request_landings (request_id, source, snapshot_id)
                 VALUES (?1, ?2, ?3)",
                params![request_id, source.unwrap_or(ROOT_MOUNT), snapshot_id],
            )
            .map_err(engine_err)?;
        Ok(())
    }

    /// The request's recorded landings: source (`None` for the root) and
    /// the snapshot it landed, in source order.
    pub fn landings(&self, request_id: i64) -> Result<Vec<(Option<String>, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT source, snapshot_id FROM request_landings
                 WHERE request_id = ?1 ORDER BY source",
            )
            .map_err(engine_err)?;
        let rows = stmt
            .query_map(params![request_id], |row| {
                let source: String = row.get(0)?;
                let snapshot: String = row.get(1)?;
                Ok(((source != ROOT_MOUNT).then_some(source), snapshot))
            })
            .map_err(engine_err)?;
        collect(rows)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn coordination() -> (tempfile::TempDir, Coordination) {
        let dir = tempfile::tempdir().unwrap();
        let coordination = Coordination::open(&dir.path().join("journal.sqlite3")).unwrap();
        (dir, coordination)
    }

    fn actor() -> Actor {
        Actor {
            name: "test-actor".to_owned(),
            kind: ActorKind::Human,
        }
    }

    #[test]
    fn landing_points_are_per_source() {
        let (_dir, coordination) = coordination();

        // Distinct sources' points never contend: two holders claim two
        // mounts' landing points at once.
        assert!(matches!(
            coordination
                .claim_lease("landing/aa", "one", 0, 1000)
                .unwrap(),
            LeaseClaim::Held
        ));
        assert!(matches!(
            coordination
                .claim_lease("landing/bb", "two", 0, 1000)
                .unwrap(),
            LeaseClaim::Held
        ));
        // One source's point still admits exactly one holder.
        assert!(matches!(
            coordination
                .claim_lease("landing/aa", "two", 0, 1000)
                .unwrap(),
            LeaseClaim::HeldByOther { .. }
        ));
    }

    #[test]
    fn a_request_move_names_its_prior_state_or_loses() {
        let (_dir, coordination) = coordination();
        let session = coordination
            .create_session(&actor(), "summary", None, None, 1)
            .unwrap();
        let request = coordination.create_request(session, &actor(), 2).unwrap();

        // A move from a state the row does not hold changes nothing.
        let stale = coordination
            .move_request_state(request, &[RequestState::Approved], RequestState::Landed)
            .unwrap();
        assert!(!stale);
        assert_eq!(
            coordination.request(request).unwrap().unwrap().state,
            RequestState::Open
        );

        // The legal transition lands; repeating it is stale and loses.
        assert!(
            coordination
                .move_request_state(request, &[RequestState::Open], RequestState::Approved)
                .unwrap()
        );
        assert!(
            !coordination
                .move_request_state(request, &[RequestState::Open], RequestState::Approved)
                .unwrap()
        );
        assert_eq!(
            coordination.request(request).unwrap().unwrap().state,
            RequestState::Approved
        );
    }

    #[test]
    fn a_request_move_accepts_any_of_its_named_priors() {
        let (_dir, coordination) = coordination();
        let session = coordination
            .create_session(&actor(), "summary", None, None, 1)
            .unwrap();
        let request = coordination.create_request(session, &actor(), 2).unwrap();

        // Open is the last entry in the from list: every placeholder binds.
        let moved = coordination
            .move_request_state(
                request,
                &[
                    RequestState::Approved,
                    RequestState::Parked,
                    RequestState::Open,
                ],
                RequestState::Abandoned,
            )
            .unwrap();
        assert!(moved);
        assert_eq!(
            coordination.request(request).unwrap().unwrap().state,
            RequestState::Abandoned
        );
    }

    #[test]
    fn a_session_move_names_its_prior_state_or_loses() {
        let (_dir, coordination) = coordination();
        let session = coordination
            .create_session(&actor(), "summary", None, None, 1)
            .unwrap();

        assert!(
            coordination
                .move_session_state(session, SessionState::Open, SessionState::Landed)
                .unwrap()
        );
        // The stale writer — abandoning a session already landed — loses.
        let stale = coordination
            .move_session_state(session, SessionState::Open, SessionState::Abandoned)
            .unwrap();
        assert!(!stale);
        assert_eq!(
            coordination.session(session).unwrap().unwrap().state,
            SessionState::Landed
        );
    }
}
