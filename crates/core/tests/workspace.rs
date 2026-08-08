use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use atelier_core::{Act, ActorKind, DeltaKind, Error, Fidelity, Workspace};

/// Serialize tests: they all set the process-wide `ATELIER_CONFIG_HOME`.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_actor(config_home: &Path) {
    fs::create_dir_all(config_home).unwrap();
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .unwrap();
    // SAFETY: every test holds `env_lock()` for its whole body, so no other
    // thread reads or writes the environment concurrently.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

fn point_config_home(config_home: &Path) {
    // SAFETY: as above; guarded by `env_lock()`.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

#[test]
fn init_creates_control_dir_journal_and_git() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();

    assert!(root.path().join(".atelier/config.toml").is_file());
    assert!(root.path().join(".atelier/journal.sqlite3").is_file());
    assert!(root.path().join(".git").exists());

    let entries = ws.journal(10).unwrap();
    assert!(entries.iter().any(|entry| {
        entry.act == Act::WorkspaceInit
            && entry.actor_name == "test-actor"
            && entry.actor_kind == ActorKind::Human
    }));
}

#[test]
fn attach_imports_files_and_records_snapshot() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), "hi").unwrap();
    fs::create_dir(source.path().join("sub")).unwrap();
    fs::write(source.path().join("sub/nested.txt"), "nested").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    assert!(root.path().join("hello.txt").is_file());
    assert!(root.path().join("sub/nested.txt").is_file());

    let entries = ws.journal(20).unwrap();
    assert!(entries.iter().any(|entry| entry.act == Act::SourceAttach));

    let log = ws.log(20).unwrap();
    assert!(!log.is_empty());
    assert!(log.iter().all(|snapshot| snapshot.actor == "test-actor"));
}

#[test]
fn edit_produces_snapshot_and_binary_changed_delta() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), "hi").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("hello.txt"), "changed").unwrap();

    let entries = ws.journal(50).unwrap();
    assert!(entries.iter().any(|entry| entry.act == Act::Snapshot));

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.fidelity, Fidelity::Binary);
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "hello.txt");
    assert_eq!(diff.deltas[0].kind, DeltaKind::Changed);
}

#[test]
fn missing_actor_config_is_reported() {
    let _guard = env_lock();
    let empty = tempfile::tempdir().unwrap();
    point_config_home(empty.path());
    let root = tempfile::tempdir().unwrap();

    let result = Workspace::init(root.path());

    assert!(matches!(result, Err(Error::NoActorConfigured)));
}

#[test]
fn attaching_an_lfs_source_is_rejected() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(
        source.path().join(".gitattributes"),
        "*.psd filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    let err = ws.attach(source.path()).unwrap_err();

    assert!(matches!(err, Error::LfsSourceUnsupported));
}

#[test]
fn boundary_paths_never_appear_in_deltas_or_log() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), "hi").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    let diff = ws.diff_latest().unwrap();
    assert!(!diff.deltas.is_empty());
    for delta in &diff.deltas {
        let address = delta.address.as_str();
        assert!(!address.starts_with(".atelier"), "leaked: {address}");
        assert!(!address.starts_with(".jj"), "leaked: {address}");
        assert!(!address.starts_with(".git"), "leaked: {address}");
    }

    for snapshot in ws.log(50).unwrap() {
        for parent in &snapshot.parents {
            assert!(!parent.is_empty());
        }
    }
}
