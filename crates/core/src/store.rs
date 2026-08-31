use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::{Error, engine_err};

/// One `SQLite` database beside the repo holds the journal (ADR-0005) and the
/// coordination state — sessions, landing requests, approvals, the landing
/// lease (ADR-0008). Every connection runs in WAL mode with a busy timeout,
/// so the CLI and a server share one lease-world across processes.
pub(crate) fn open_connection(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(path).map_err(engine_err)?;
    conn.pragma_update_and_check(None, "journal_mode", "wal", |_row| Ok(()))
        .map_err(engine_err)?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(engine_err)?;
    ensure_schema(&conn)?;
    Ok(conn)
}

fn ensure_schema(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
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
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            actor_name TEXT NOT NULL,
            actor_kind TEXT NOT NULL,
            instruction_summary TEXT NOT NULL,
            instruction_run_ref TEXT,
            instruction_verbatim TEXT,
            change_id TEXT,
            state TEXT NOT NULL,
            opened_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS landing_requests (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id INTEGER NOT NULL,
            requester_name TEXT NOT NULL,
            requester_kind TEXT NOT NULL,
            state TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS approvals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id INTEGER NOT NULL,
            actor_name TEXT NOT NULL,
            actor_kind TEXT NOT NULL,
            snapshot_id TEXT NOT NULL,
            at_ms INTEGER NOT NULL,
            dismissed INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS session_changes (
            session_id INTEGER NOT NULL,
            source TEXT NOT NULL,
            change_id TEXT NOT NULL,
            PRIMARY KEY (session_id, source)
        );
        CREATE TABLE IF NOT EXISTS request_landings (
            request_id INTEGER NOT NULL,
            source TEXT NOT NULL,
            snapshot_id TEXT NOT NULL,
            PRIMARY KEY (request_id, source)
        );
        CREATE TABLE IF NOT EXISTS lease (
            point TEXT PRIMARY KEY,
            holder TEXT NOT NULL,
            expires_at_ms INTEGER NOT NULL,
            epoch INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS sync_state (
            mount TEXT PRIMARY KEY,
            fingerprint TEXT NOT NULL,
            snapshot_id TEXT NOT NULL
        );",
    )
    .map_err(engine_err)?;
    migrate_lease_epoch(conn)?;
    conn.pragma_update(None, "user_version", 6)
        .map_err(engine_err)?;
    Ok(())
}

/// A store from before the fenced lease carries an epoch-less `lease`
/// table, and `CREATE IF NOT EXISTS` cannot add the column. `DEFAULT 0`
/// keeps every real tenancy (epochs count from 1) newer than any
/// pre-fence row.
fn migrate_lease_epoch(conn: &Connection) -> Result<(), Error> {
    let fenced: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('lease') WHERE name = 'epoch'",
            [],
            |row| row.get(0),
        )
        .map_err(engine_err)?;
    if fenced == 0 {
        conn.execute_batch("ALTER TABLE lease ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0")
            .map_err(engine_err)?;
    }
    Ok(())
}
