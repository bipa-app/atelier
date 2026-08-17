//! The rustyriver conformance harness (ADR-0013, H1): a live workspace's
//! store replicates while acts happen, a restore into a fresh path arrives
//! row-for-row whole, and a point-in-time restore proves an earlier state.
//! This suite gates every hosted slice: if the wheel wobbles here, H2 and
//! H3 do not start.

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_hosted::{StoreReplica, latest_txid, restore_to};
use atelier_sdk::{Actor, ActorKind, GateOutcome, Instruction, Workspace};

/// Serialize tests: they all set the process-wide `ATELIER_CONFIG_HOME`.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[expect(unsafe_code, reason = "set_var wires the workspace to the test config")]
fn set_actor(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
    // SAFETY: every test holds `env_lock()` for its whole body, so no other
    // thread reads or writes the environment concurrently.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

fn actor() -> Actor {
    Actor {
        name: "scribe".to_owned(),
        kind: ActorKind::Agent,
    }
}

fn instruction() -> Instruction {
    Instruction {
        summary: "work worth replicating".to_owned(),
        run_ref: None,
        verbatim: None,
    }
}

/// The workspace store's own tables, every row rendered, ordered by rowid.
/// rustyriver's `_litestream_*` control tables are its own and excluded.
fn table_rows(db: &Path, table: &str) -> Vec<String> {
    let connection = rusqlite::Connection::open(db).expect("open the store");
    let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .expect("prepare the scan");
    let columns = statement.column_count();
    let rows = statement
        .query_map([], |row| {
            let mut rendered = Vec::with_capacity(columns);
            for index in 0..columns {
                rendered.push(format!(
                    "{:?}",
                    row.get::<_, rusqlite::types::Value>(index)?
                ));
            }
            Ok(rendered.join("|"))
        })
        .expect("scan the table");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("render the rows")
}

const STORE_TABLES: [&str; 8] = [
    "journal",
    "sessions",
    "session_changes",
    "landing_requests",
    "approvals",
    "request_landings",
    "lease",
    "sync_state",
];

#[test]
fn a_live_store_replicates_and_restores_whole() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let store = root.path().join(".atelier").join("journal.sqlite3");
    let replica = tempfile::tempdir().unwrap();

    // Replication starts against a store that is already alive.
    let origin = tempfile::tempdir().unwrap();
    fs::write(origin.path().join("notes.txt"), "the note\n").unwrap();
    ws.attach(origin.path()).unwrap();
    let mut replication = StoreReplica::open(&store, replica.path()).unwrap();
    replication.sync().unwrap();
    let checkpoint = latest_txid(replica.path()).unwrap().expect("a first txid");
    let journal_rows_at_checkpoint = table_rows(&store, "journal").len();

    // The workspace keeps working after the checkpoint: a session lands
    // (which also mirrors the origin and records sync state).
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "the revised note\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    replication.sync().unwrap();

    // The latest restore arrives row-for-row whole, table by table.
    let restored = tempfile::tempdir().unwrap();
    let latest = restored.path().join("journal.sqlite3");
    restore_to(replica.path(), &latest, None).unwrap();
    for table in STORE_TABLES {
        assert_eq!(
            table_rows(&store, table),
            table_rows(&latest, table),
            "table {table} diverged in the restore"
        );
    }
    // The restored journal really carries the arc, not just row counts.
    let acts: Vec<String> = table_rows(&latest, "journal");
    assert!(
        acts.iter().any(|row| row.contains("land")),
        "the landing act crossed the replica: {acts:?}"
    );

    // The point-in-time restore proves the earlier state: the journal as
    // it stood at the checkpoint, before the session ever opened.
    let earlier = restored.path().join("checkpoint.sqlite3");
    restore_to(replica.path(), &earlier, Some(checkpoint)).unwrap();
    assert_eq!(
        table_rows(&earlier, "journal").len(),
        journal_rows_at_checkpoint,
        "the checkpoint restore is the pre-session store"
    );
    assert_ne!(
        table_rows(&earlier, "journal").len(),
        table_rows(&latest, "journal").len(),
        "the two restores are distinct states"
    );

    // Attribution crossed the replica whole: the configured human's acts
    // and the session agent's both read back by name.
    let journal_acts = table_rows(&latest, "journal");
    assert!(journal_acts.iter().any(|row| row.contains("test-actor")));
    assert!(journal_acts.iter().any(|row| row.contains("scribe")));
}
