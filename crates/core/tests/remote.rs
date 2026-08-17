//! Remote sources (ADR-0012) over the file:// scheme — the same adapter
//! code path real buckets take, minus the network: attach imports the
//! objects, a landing mirrors home under the listing fingerprint, a
//! bucket moved out-of-band parks, and force reconciles.

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_sdk::{
    Act, Actor, ActorKind, Error, GateOutcome, Instruction, PullOutcome, SyncOutcome, Workspace,
};

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
        summary: "revise the bucket".to_owned(),
        run_ref: None,
        verbatim: None,
    }
}

fn bucket_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

#[test]
fn a_bucket_attaches_lands_and_mirrors_home() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let bucket = tempfile::tempdir().unwrap();
    fs::write(bucket.path().join("contract.md"), "# Draft\n").unwrap();
    fs::create_dir(bucket.path().join("annexes")).unwrap();
    fs::write(bucket.path().join("annexes").join("a.md"), "annex a\n").unwrap();

    let source = ws
        .attach_remote(&bucket_url(bucket.path()), "docs")
        .unwrap();
    assert_eq!(source.kind.to_string(), "remote");

    // The objects imported into the mount, nested keys as paths.
    assert_eq!(
        fs::read_to_string(root.path().join("docs").join("contract.md")).unwrap(),
        "# Draft\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("docs").join("annexes").join("a.md")).unwrap(),
        "annex a\n"
    );

    // The manifest names the remote source.
    let manifest = ws.manifest().unwrap();
    assert!(
        manifest.contains(&format!(
            "docs  remote  {}  two-way",
            bucket_url(bucket.path())
        )),
        "got: {manifest}"
    );

    // A landing mirrors home: update, add, remove.
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/contract.md", "# Signed\n")
        .unwrap();
    ws.session_write(session.id, "docs/annexes/b.md", "annex b\n")
        .unwrap();
    fs::remove_file(
        session
            .working_copy
            .join("docs")
            .join("annexes")
            .join("a.md"),
    )
    .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");

    assert_eq!(
        fs::read_to_string(bucket.path().join("contract.md")).unwrap(),
        "# Signed\n"
    );
    assert_eq!(
        fs::read_to_string(bucket.path().join("annexes").join("b.md")).unwrap(),
        "annex b\n"
    );
    assert!(!bucket.path().join("annexes").join("a.md").exists());
    let syncs = ws
        .journal(50)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::Sync)
        .count();
    assert_eq!(syncs, 1);
}

#[test]
fn a_bucket_moved_out_of_band_parks_and_force_reconciles() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let bucket = tempfile::tempdir().unwrap();
    fs::write(bucket.path().join("doc.md"), "v1\n").unwrap();
    ws.attach_remote(&bucket_url(bucket.path()), "docs")
        .unwrap();

    // A colleague uploads after the attach.
    fs::write(bucket.path().join("doc.md"), "the colleague's v2\n").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/doc.md", "the session's v2\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");

    // The landing stood; the bucket was never overwritten; the park is
    // journaled by name.
    assert_eq!(
        fs::read_to_string(bucket.path().join("doc.md")).unwrap(),
        "the colleague's v2\n"
    );
    let parked = ws
        .journal(50)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::SyncParked)
        .count();
    assert_eq!(parked, 1);

    // A plain retry parks again; force overwrites deliberately and seeds
    // the fingerprint, so the next landing syncs cleanly.
    let retry = ws.sync(Some("docs"), false).unwrap();
    assert!(matches!(retry, SyncOutcome::Parked { .. }), "{retry:?}");
    let forced = ws.sync(Some("docs"), true).unwrap();
    assert!(matches!(forced, SyncOutcome::Synced { .. }), "{forced:?}");
    assert_eq!(
        fs::read_to_string(bucket.path().join("doc.md")).unwrap(),
        "the session's v2\n"
    );

    let second = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(second.id, "docs/doc.md", "v3\n").unwrap();
    let outcome = ws.land(second.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(bucket.path().join("doc.md")).unwrap(),
        "v3\n"
    );
}

#[test]
fn remote_refusals_speak_by_name() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    // A bucket carrying engine-internal names refuses to import.
    let bucket = tempfile::tempdir().unwrap();
    fs::create_dir(bucket.path().join(".git")).unwrap();
    fs::write(bucket.path().join(".git").join("config"), "smuggled\n").unwrap();
    let error = ws
        .attach_remote(&bucket_url(bucket.path()), "docs")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Engine(message) if message.contains("engine-internal name")),
        "got: {error:?}"
    );

    // The failed attach left no mount behind.
    assert!(!root.path().join("docs").exists() || ws.manifest().unwrap().contains("(none)"));

    // A mount name collision refuses the same way local sources do.
    let clean = tempfile::tempdir().unwrap();
    fs::write(clean.path().join("a.md"), "a\n").unwrap();
    ws.attach_remote(&bucket_url(clean.path()), "docs").unwrap();
    let error = ws
        .attach_remote(&bucket_url(clean.path()), "docs")
        .unwrap_err();
    assert!(matches!(error, Error::AlreadyAttached), "got: {error:?}");

    // An undo steps the line back and re-mirrors the bucket.
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/a.md", "revised\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(clean.path().join("a.md")).unwrap(),
        "revised\n"
    );
    ws.undo("r1".parse().unwrap()).unwrap();
    assert_eq!(
        fs::read_to_string(clean.path().join("a.md")).unwrap(),
        "a\n"
    );
}

#[test]
fn a_pull_folds_bucket_changes_into_the_line() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let bucket = tempfile::tempdir().unwrap();
    fs::write(bucket.path().join("keep.md"), "kept\n").unwrap();
    fs::write(bucket.path().join("old.md"), "old\n").unwrap();
    ws.attach_remote(&bucket_url(bucket.path()), "docs")
        .unwrap();

    // Nothing changed yet: the pull says so and moves nothing.
    assert_eq!(ws.pull(Some("docs")).unwrap(), PullOutcome::Current);

    // A colleague updates, adds, and removes objects.
    fs::write(bucket.path().join("keep.md"), "kept, revised\n").unwrap();
    fs::write(bucket.path().join("new.md"), "brand new\n").unwrap();
    fs::remove_file(bucket.path().join("old.md")).unwrap();

    let outcome = ws.pull(Some("docs")).unwrap();
    let PullOutcome::Pulled { snapshot } = outcome else {
        panic!("the pull must fold, got {outcome:?}");
    };

    // The mount's line advanced to the bucket's state, one attributed act.
    assert_eq!(
        fs::read_to_string(root.path().join("docs").join("keep.md")).unwrap(),
        "kept, revised\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("docs").join("new.md")).unwrap(),
        "brand new\n"
    );
    assert!(!root.path().join("docs").join("old.md").exists());
    let pulls: Vec<String> = ws
        .journal(50)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::Pull)
        .map(|entry| entry.reference.unwrap_or_default())
        .collect();
    assert_eq!(pulls, vec![format!("docs {snapshot}")]);

    // The fingerprint stayed coherent: the full cycle keeps working.
    // pull -> session -> land mirrors home with no park.
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/keep.md", "kept, landed\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(bucket.path().join("keep.md")).unwrap(),
        "kept, landed\n"
    );
    let parked = ws
        .journal(50)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::SyncParked)
        .count();
    assert_eq!(parked, 0);
    assert_eq!(ws.pull(Some("docs")).unwrap(), PullOutcome::Current);
}

#[test]
fn a_pull_refuses_local_line_movement_by_name() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let bucket = tempfile::tempdir().unwrap();
    fs::write(bucket.path().join("doc.md"), "v1\n").unwrap();
    ws.attach_remote(&bucket_url(bucket.path()), "docs")
        .unwrap();

    // The bucket moves - and so does the local line, out-of-band: the
    // pull's own auto-snapshot captures the local edit as line movement.
    fs::write(bucket.path().join("doc.md"), "the colleague's v2\n").unwrap();
    fs::write(root.path().join("docs").join("doc.md"), "a local edit\n").unwrap();

    let error = ws.pull(Some("docs")).unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("moved locally since its last sync")),
        "got: {error:?}"
    );
    // Nothing was pulled over the local edit; both states survive.
    assert_eq!(
        fs::read_to_string(root.path().join("docs").join("doc.md")).unwrap(),
        "a local edit\n"
    );
    assert_eq!(
        fs::read_to_string(bucket.path().join("doc.md")).unwrap(),
        "the colleague's v2\n"
    );

    // Landing the local movement re-parks against the moved bucket (the
    // sync guard), force reconciles, and then the pull is current.
    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/doc.md", "a local edit, landed\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");
    let forced = ws.sync(Some("docs"), true).unwrap();
    assert!(matches!(forced, SyncOutcome::Synced { .. }), "{forced:?}");
    assert_eq!(ws.pull(Some("docs")).unwrap(), PullOutcome::Current);

    // Non-remote sources refuse the verb by name.
    let folder = tempfile::tempdir().unwrap();
    fs::write(folder.path().join("f.txt"), "f\n").unwrap();
    ws.attach_mount(folder.path(), "files").unwrap();
    let error = ws.pull(Some("files")).unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("not a remote source")),
        "got: {error:?}"
    );
}
