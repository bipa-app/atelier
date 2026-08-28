//! Claim, hydrate, serve, release (ADR-0013, H3/H4): a node claims a
//! workspace and replicates real work under its epoch; killed without a
//! release, a second node takes over, hydrates from the surviving
//! lineage — the `SQLite` store and the jj/git stores from one completed
//! pass — rematerializes working copies, and serves every acknowledged
//! act; a deposed node's replication surfaces refusal; a release hands
//! the workspace to a plain claim. The ownership plane rides a shared
//! in-memory store (the conditional writes real buckets grant);
//! replication rides a file-backed replica area — in production one
//! bucket carries both planes through `open_planes`.

use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use atelier_hosted::object_store::ObjectStore;
use atelier_hosted::object_store::memory::InMemory;
use atelier_hosted::object_store::path::Path as ObjectPath;
use atelier_hosted::{
    HostedNode, NodeClaim, NodePaths, Ownership, OwnershipRecord, ReleaseOutcome, ReplicaArea,
    ReplicateOutcome, latest_txid, restore_to,
};
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
        summary: "work worth failing over".to_owned(),
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

/// One shared ownership plane — what the bucket is to every node.
fn plane() -> (Arc<dyn ObjectStore>, ObjectPath) {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    (store, ObjectPath::from("cells/ws1"))
}

/// One node's handle onto the shared plane.
fn handle(store: &Arc<dyn ObjectStore>, prefix: &ObjectPath) -> Ownership {
    Ownership::from_store(Arc::clone(store), prefix.clone()).expect("open the plane")
}

/// A minimal store standing in for a workspace's: a hosted node
/// replicates whatever `SQLite` database it is handed.
fn seed_store(path: &Path) {
    let connection = rusqlite::Connection::open(path).expect("open the store");
    connection
        .execute("CREATE TABLE acts (detail TEXT NOT NULL)", [])
        .expect("create the table");
    connection
        .execute("INSERT INTO acts (detail) VALUES ('the first act')", [])
        .expect("record the act");
}

fn record_act(path: &Path, detail: &str) {
    let connection = rusqlite::Connection::open(path).expect("open the store");
    connection
        .execute("INSERT INTO acts (detail) VALUES (?1)", [detail])
        .expect("record the act");
}

/// The one serving node an activation must produce.
fn serving(claim: NodeClaim) -> HostedNode {
    match claim {
        NodeClaim::Serving(node) => Some(*node),
        NodeClaim::HeldByOther { .. } => None,
    }
    .expect("the activation must serve")
}

/// The canonical paths a node serves a real workspace at.
fn workspace_paths(root: &Path, replica_root: &Path) -> NodePaths {
    NodePaths {
        store: root.join(".atelier").join("journal.sqlite3"),
        root: root.to_path_buf(),
        replica: ReplicaArea::Files(replica_root.to_path_buf()),
    }
}

/// A git repository fixture with two commits; the two ids, oldest first.
fn git_repo(dir: &Path) -> Vec<String> {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "upstream")
            .env("GIT_AUTHOR_EMAIL", "upstream@example.com")
            .env("GIT_COMMITTER_NAME", "upstream")
            .env("GIT_COMMITTER_EMAIL", "upstream@example.com")
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).expect("git output is utf-8")
    };
    git(&["init", "-q", "-b", "master", "."]);
    fs::write(dir.join("lib.rs"), "pub fn lib() {}\n").expect("write repo file");
    git(&["add", "."]);
    git(&["commit", "-qm", "the pre-attach commit"]);
    let first = git(&["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(dir.join("README.md"), "readme\n").expect("write repo file");
    git(&["add", "."]);
    git(&["commit", "-qm", "second pre-attach commit"]);
    let second = git(&["rev-parse", "HEAD"]).trim().to_owned();
    vec![first, second]
}

#[test]
fn a_killed_owner_fails_over_whole() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let (bucket, prefix) = plane();
    let replica_root = tempfile::tempdir().unwrap();

    // Node A claims the fresh workspace: its live store seeds epoch 1.
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let origin = tempfile::tempdir().unwrap();
    fs::write(origin.path().join("notes.txt"), "the note\n").unwrap();
    ws.attach(origin.path()).unwrap();
    let paths_a = workspace_paths(root.path(), replica_root.path());
    let store_a = paths_a.store.clone();
    let mut node_a =
        serving(HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths_a).unwrap());
    assert_eq!(node_a.epoch(), 1);

    // Real work lands, and A's replication is acknowledged.
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "the revised note\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    assert_eq!(node_a.replicate().unwrap(), ReplicateOutcome::Acknowledged);
    let acknowledged: Vec<Vec<String>> = STORE_TABLES
        .iter()
        .map(|table| table_rows(&store_a, table))
        .collect();

    // The kill: node A vanishes without releasing.
    drop(node_a);

    // Node B seizes, hydrates from A's lineage, and serves every
    // acknowledged act row-for-row.
    let b_root = tempfile::tempdir().unwrap();
    let paths_b = workspace_paths(b_root.path(), replica_root.path());
    let mut node_b =
        serving(HostedNode::take_over(handle(&bucket, &prefix), "node-b", &paths_b).unwrap());
    assert_eq!(node_b.epoch(), 2);
    for (table, rows) in STORE_TABLES.iter().zip(&acknowledged) {
        assert_eq!(
            &table_rows(&paths_b.store, table),
            rows,
            "table {table} diverged across the failover"
        );
    }
    let acts: Vec<String> = table_rows(&paths_b.store, "journal");
    assert!(
        acts.iter().any(|row| row.contains("land")),
        "the landing act survived the failover: {acts:?}"
    );

    // The workspace opens whole on B: working copies rematerialize from
    // the hydrated stores, and the landed content is on disk.
    let mut ws_b = Workspace::rematerialize(b_root.path()).unwrap();
    assert_eq!(
        fs::read_to_string(b_root.path().join("notes.txt")).unwrap(),
        "the revised note\n"
    );
    let history = ws_b.log(50).unwrap();
    assert!(
        history.iter().any(|entry| entry.snapshot.actor == "scribe"),
        "the landed snapshot survived: {history:?}"
    );

    // B replicates under its own epoch, and its lineage stands alone: a
    // restore from e2 alone rebuilds B's whole store.
    assert_eq!(node_b.replicate().unwrap(), ReplicateOutcome::Acknowledged);
    let lineage_b = replica_root.path().join("e2");
    assert!(latest_txid(&lineage_b).unwrap().is_some());
    let restored = b_root.path().join("restored.sqlite3");
    restore_to(&lineage_b, &restored, None).unwrap();
    for table in STORE_TABLES {
        assert_eq!(
            table_rows(&restored, table),
            table_rows(&paths_b.store, table),
            "table {table} diverged in B's own lineage"
        );
    }

    // The acknowledgement rule: A's identity can no longer confirm, B's can.
    let plane_view = handle(&bucket, &prefix);
    assert!(!plane_view.confirm("node-a", 1).unwrap());
    assert!(plane_view.confirm("node-b", 2).unwrap());
}

#[test]
fn a_git_mounted_workspace_survives_failover_whole() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let (bucket, prefix) = plane();
    let replica_root = tempfile::tempdir().unwrap();

    // A workspace with an adopted git repo and an open mid-flight session.
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let pre_attach = git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { work() }\n")
        .unwrap();

    let paths_a = workspace_paths(root.path(), replica_root.path());
    let mut node_a =
        serving(HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths_a).unwrap());
    assert_eq!(node_a.replicate().unwrap(), ReplicateOutcome::Acknowledged);
    drop(node_a);
    drop(ws);

    // Another node seizes and loads the workspace from the bucket: the
    // adopted history, the mount's working copy, and the open session all
    // rematerialize.
    let b_root = tempfile::tempdir().unwrap();
    let paths_b = workspace_paths(b_root.path(), replica_root.path());
    let node_b =
        serving(HostedNode::take_over(handle(&bucket, &prefix), "node-b", &paths_b).unwrap());
    let mut ws_b = Workspace::rematerialize(b_root.path()).unwrap();
    assert_eq!(
        fs::read_to_string(b_root.path().join("sdk").join("lib.rs")).unwrap(),
        "pub fn lib() {}\n"
    );

    // The adopted history is intact for plain git in the hydrated mount.
    let git_log = std::process::Command::new("git")
        .args(["log", "--format=%H"])
        .current_dir(b_root.path().join("sdk"))
        .output()
        .expect("run git log");
    assert!(git_log.status.success(), "{git_log:?}");
    let seen = String::from_utf8(git_log.stdout).expect("git log is utf-8");
    assert!(
        seen.contains(&pre_attach[0]) && seen.contains(&pre_attach[1]),
        "git log lost the adopted history: {seen}"
    );

    // The session picks up exactly where it stood: its unlanded write is
    // in its rematerialized working copy, and it lands on the new node.
    let diff = ws_b.session_diff(session.id).unwrap();
    assert!(
        !diff.deltas.is_empty(),
        "the session's unlanded work survived the failover"
    );
    let outcome = ws_b.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(b_root.path().join("sdk").join("lib.rs")).unwrap(),
        "pub fn lib() { work() }\n"
    );

    // The landed history and the release both belong to B now.
    assert_eq!(node_b.release().unwrap(), ReleaseOutcome::Released);
}

#[test]
fn a_deposed_node_surfaces_refusal_and_keeps_writing() {
    let (bucket, prefix) = plane();
    let replica_root = tempfile::tempdir().unwrap();
    let a_root = tempfile::tempdir().unwrap();
    let paths_a = NodePaths {
        store: a_root.path().join("store.sqlite3"),
        root: a_root.path().to_path_buf(),
        replica: ReplicaArea::Files(replica_root.path().to_path_buf()),
    };
    seed_store(&paths_a.store);
    let mut node_a =
        serving(HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths_a).unwrap());
    assert_eq!(node_a.replicate().unwrap(), ReplicateOutcome::Acknowledged);
    let acknowledged = latest_txid(&replica_root.path().join("e1"))
        .unwrap()
        .expect("a first acknowledged transaction");

    // A plain claim by another node refuses by name — no seizure.
    let refused = HostedNode::claim(
        handle(&bucket, &prefix),
        "node-b",
        &NodePaths {
            store: a_root.path().join("unused.sqlite3"),
            root: a_root.path().to_path_buf(),
            replica: ReplicaArea::Files(replica_root.path().to_path_buf()),
        },
    )
    .unwrap();
    match refused {
        NodeClaim::HeldByOther { holder, epoch } => {
            assert_eq!(holder, "node-a");
            assert_eq!(epoch, 1);
        }
        NodeClaim::Serving(_) => panic!("a plain claim never seizes"),
    }

    // Elsewhere, the workspace is seized. A keeps writing — plain PUTs
    // never refuse — but its replication now surfaces the refusal.
    handle(&bucket, &prefix).take_over("node-b").unwrap();
    record_act(&paths_a.store, "a late act");
    assert_eq!(node_a.replicate().unwrap(), ReplicateOutcome::Deposed);
    let late = latest_txid(&replica_root.path().join("e1"))
        .unwrap()
        .expect("the late transaction still uploaded");
    assert!(
        late.0 > acknowledged.0,
        "the superseded lineage kept the bytes: {late:?} over {acknowledged:?}"
    );

    // Deposed, A's release is moot.
    assert_eq!(node_a.release().unwrap(), ReleaseOutcome::NotHeld);
}

#[test]
fn a_release_hands_the_workspace_to_a_plain_claim() {
    let (bucket, prefix) = plane();
    let replica_root = tempfile::tempdir().unwrap();
    let a_root = tempfile::tempdir().unwrap();
    let paths_a = NodePaths {
        store: a_root.path().join("store.sqlite3"),
        root: a_root.path().to_path_buf(),
        replica: ReplicaArea::Files(replica_root.path().to_path_buf()),
    };
    seed_store(&paths_a.store);
    let node_a = serving(HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths_a).unwrap());

    // The release captures outstanding acts first: nothing is lost even
    // though A never replicated by hand.
    record_act(&paths_a.store, "an act only the final capture carries");
    assert_eq!(node_a.release().unwrap(), ReleaseOutcome::Released);
    assert_eq!(
        handle(&bucket, &prefix).record().unwrap(),
        Some(OwnershipRecord {
            holder: None,
            epoch: 1
        })
    );

    // Any node now claims plainly and serves the released state whole.
    let b_root = tempfile::tempdir().unwrap();
    let paths_b = NodePaths {
        store: b_root.path().join("store.sqlite3"),
        root: b_root.path().to_path_buf(),
        replica: ReplicaArea::Files(replica_root.path().to_path_buf()),
    };
    let node_b = serving(HostedNode::claim(handle(&bucket, &prefix), "node-b", &paths_b).unwrap());
    assert_eq!(node_b.epoch(), 2);
    assert_eq!(
        table_rows(&paths_b.store, "acts"),
        vec![
            "Text(\"the first act\")".to_owned(),
            "Text(\"an act only the final capture carries\")".to_owned(),
        ]
    );
    assert_eq!(node_b.release().unwrap(), ReleaseOutcome::Released);
}

#[test]
fn an_activation_without_a_store_or_lineage_refuses() {
    let (bucket, prefix) = plane();
    let replica_root = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = NodePaths {
        store: root.path().join("absent.sqlite3"),
        root: root.path().to_path_buf(),
        replica: ReplicaArea::Files(replica_root.path().to_path_buf()),
    };
    let error = HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths)
        .err()
        .expect("nothing to serve");
    assert_eq!(
        error.to_string(),
        "the workspace has no store to serve: no local store, no lineage"
    );
}

#[test]
fn a_local_store_never_shadows_a_lineage() {
    let (bucket, prefix) = plane();
    let replica_root = tempfile::tempdir().unwrap();
    let a_root = tempfile::tempdir().unwrap();
    let paths_a = NodePaths {
        store: a_root.path().join("store.sqlite3"),
        root: a_root.path().to_path_buf(),
        replica: ReplicaArea::Files(replica_root.path().to_path_buf()),
    };
    seed_store(&paths_a.store);
    let node_a = serving(HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths_a).unwrap());
    assert_eq!(node_a.release().unwrap(), ReleaseOutcome::Released);

    // Re-activating over a leftover local store refuses: hosted state is
    // derived from the bucket, never trusted from disk.
    let error = HostedNode::claim(handle(&bucket, &prefix), "node-a", &paths_a)
        .err()
        .expect("the shadow must refuse");
    assert_eq!(
        error.to_string(),
        "the local store would shadow the bucket's lineage; remove it and hydrate"
    );
}
