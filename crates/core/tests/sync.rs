//! Sync-back (ADR-0010): a landing on a folder-sourced line mirrors the
//! landed tree to the origin, guarded by the recorded fingerprint; an
//! origin edited out-of-band parks the sync — journaled, never destroyed.

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_sdk::{Act, Actor, ActorKind, Error, GateOutcome, Instruction, SyncOutcome, Workspace};

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
        summary: "revise the folder".to_owned(),
        run_ref: None,
        verbatim: None,
    }
}

fn acts_of(ws: &mut Workspace, act: Act) -> Vec<String> {
    ws.journal(100)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == act)
        .map(|entry| entry.reference.unwrap_or_default())
        .collect()
}

#[test]
fn a_landing_mirrors_the_root_import_home() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let origin = tempfile::tempdir().unwrap();
    fs::write(origin.path().join("a.txt"), "a\n").unwrap();
    fs::create_dir(origin.path().join("sub")).unwrap();
    fs::write(origin.path().join("sub").join("old.txt"), "old\n").unwrap();
    ws.attach(origin.path()).unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "a.txt", "a revised\n")
        .unwrap();
    ws.session_write(session.id, "new.txt", "brand new\n")
        .unwrap();
    fs::remove_file(session.working_copy.join("sub").join("old.txt")).unwrap();

    let outcome = ws.land(session.id).unwrap();
    let GateOutcome::Landed { landings } = outcome else {
        panic!("the land must land, got {outcome:?}");
    };

    // The origin mirrors the landed tree: updated, added, removed.
    assert_eq!(
        fs::read_to_string(origin.path().join("a.txt")).unwrap(),
        "a revised\n"
    );
    assert_eq!(
        fs::read_to_string(origin.path().join("new.txt")).unwrap(),
        "brand new\n"
    );
    assert!(!origin.path().join("sub").exists());

    // The journal records the sync against the landed snapshot.
    assert_eq!(
        acts_of(&mut ws, Act::Sync),
        vec![landings[0].snapshot.clone()]
    );
    assert_eq!(acts_of(&mut ws, Act::SyncParked), Vec::<String>::new());
}

#[test]
fn an_origin_edited_out_of_band_parks_and_force_overwrites() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let origin = tempfile::tempdir().unwrap();
    fs::write(origin.path().join("notes.txt"), "shared\n").unwrap();
    ws.attach_mount(origin.path(), "docs").unwrap();

    // A human edits the origin directly after the attach.
    fs::write(origin.path().join("notes.txt"), "the human's version\n").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/notes.txt", "the session's version\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    let GateOutcome::Landed { landings } = outcome else {
        panic!("the land must land, got {outcome:?}");
    };
    let landed = landings[1].snapshot.clone();

    // The landing stood; the origin was never overwritten; the park is
    // journaled by name.
    assert_eq!(
        fs::read_to_string(origin.path().join("notes.txt")).unwrap(),
        "the human's version\n"
    );
    assert_eq!(
        acts_of(&mut ws, Act::SyncParked),
        vec![format!("docs {landed} origin changed")]
    );

    // A plain retry parks again - the fingerprint still does not match.
    let retry = ws.sync(Some("docs"), false).unwrap();
    assert_eq!(
        retry,
        SyncOutcome::Parked {
            snapshot: landed.clone()
        }
    );
    assert_eq!(
        fs::read_to_string(origin.path().join("notes.txt")).unwrap(),
        "the human's version\n"
    );

    // Force overwrites deliberately, journaled; the next landing syncs
    // cleanly because the fingerprint is seeded.
    let forced = ws.sync(Some("docs"), true).unwrap();
    assert_eq!(
        forced,
        SyncOutcome::Synced {
            snapshot: landed.clone()
        }
    );
    assert_eq!(
        fs::read_to_string(origin.path().join("notes.txt")).unwrap(),
        "the session's version\n"
    );
    assert_eq!(acts_of(&mut ws, Act::Sync), vec![format!("docs {landed}")]);

    let second = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(second.id, "docs/notes.txt", "a second revision\n")
        .unwrap();
    let outcome = ws.land(second.id).unwrap();
    let GateOutcome::Landed { landings } = outcome else {
        panic!("the second land must land, got {outcome:?}");
    };
    let second_landed = landings[1].snapshot.clone();
    assert_eq!(
        fs::read_to_string(origin.path().join("notes.txt")).unwrap(),
        "a second revision\n"
    );
    assert_eq!(
        acts_of(&mut ws, Act::Sync),
        vec![format!("docs {second_landed}"), format!("docs {landed}")]
    );
}

#[test]
fn git_sources_never_sync_back() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .env("GIT_AUTHOR_NAME", "upstream")
            .env("GIT_AUTHOR_EMAIL", "upstream@example.com")
            .env("GIT_COMMITTER_NAME", "upstream")
            .env("GIT_COMMITTER_EMAIL", "upstream@example.com")
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&["init", "-q", "-b", "master", "."]);
    fs::write(repo.path().join("lib.rs"), "pub fn lib() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "the pre-attach commit"]);
    ws.attach_mount(repo.path(), "sdk").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { push() }\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }), "{outcome:?}");

    // No sync act of either kind; the origin's file is untouched.
    assert_eq!(acts_of(&mut ws, Act::Sync), Vec::<String>::new());
    assert_eq!(acts_of(&mut ws, Act::SyncParked), Vec::<String>::new());
    assert_eq!(
        fs::read_to_string(repo.path().join("lib.rs")).unwrap(),
        "pub fn lib() {}\n"
    );

    // The sync verb refuses git sources by name.
    let error = ws.sync(Some("sdk"), false).unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("git source")),
        "got: {error:?}"
    );
}
