use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_core::{Error, Workspace};

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

fn folder_with(name: &str, content: &str) -> tempfile::TempDir {
    let folder = tempfile::tempdir().expect("create source tempdir");
    fs::write(folder.path().join(name), content).expect("write source file");
    folder
}

#[test]
fn mounted_sources_carry_their_own_histories() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let app = folder_with("main.rs", "fn main() {}\n");
    let docs = folder_with("guide.md", "# Guide\n");
    ws.attach_mount(app.path(), "app").unwrap();
    ws.attach_mount(docs.path(), "docs").unwrap();
    fs::write(root.path().join("plan.md"), "# The plan\n").unwrap();

    // Every source's snapshots live in its own history; ids never collide
    // and root entries keep the exact v1 shape (no source).
    let log = ws.log(50).unwrap();
    let sources: BTreeSet<Option<&str>> = log.iter().map(|e| e.source.as_deref()).collect();
    assert_eq!(
        sources,
        BTreeSet::from([None, Some("app"), Some("docs")]),
        "unexpected sources: {sources:?}"
    );
    let ids: Vec<&str> = log.iter().map(|e| e.snapshot.id.as_str()).collect();
    let distinct: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), distinct.len(), "snapshot ids collided: {ids:?}");

    // An edit in one mount advances only that mount's history.
    let app_head = |ws: &mut Workspace| {
        ws.log(1)
            .unwrap()
            .into_iter()
            .find(|e| e.source.as_deref() == Some("app"))
            .expect("app has a history")
            .snapshot
            .id
    };
    let docs_head_before = ws
        .log(1)
        .unwrap()
        .into_iter()
        .find(|e| e.source.as_deref() == Some("docs"))
        .expect("docs has a history")
        .snapshot
        .id;
    let app_head_before = app_head(&mut ws);
    fs::write(
        root.path().join("app").join("main.rs"),
        "fn main() { run() }\n",
    )
    .unwrap();
    let app_head_after = app_head(&mut ws);
    let docs_head_after = ws
        .log(1)
        .unwrap()
        .into_iter()
        .find(|e| e.source.as_deref() == Some("docs"))
        .expect("docs has a history")
        .snapshot
        .id;
    assert_ne!(app_head_before, app_head_after);
    assert_eq!(docs_head_before, docs_head_after);
}

#[test]
fn the_aggregate_diff_scopes_addresses_by_mount() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let app = folder_with("main.rs", "fn main() {}\n");
    ws.attach_mount(app.path(), "app").unwrap();
    fs::write(root.path().join("plan.md"), "# The plan\n").unwrap();
    ws.journal(1).unwrap();

    fs::write(root.path().join("plan.md"), "# The plan\n\nrevised\n").unwrap();
    fs::write(
        root.path().join("app").join("main.rs"),
        "fn main() { run() }\n",
    )
    .unwrap();

    let diff = ws.diff_latest().unwrap();
    let addresses: Vec<&str> = diff
        .deltas
        .iter()
        .map(|delta| delta.address.as_str())
        .collect();
    assert_eq!(addresses, vec!["plan.md", "app/main.rs"]);
}

#[test]
fn root_and_mount_content_never_cross_boundaries() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let app = folder_with("main.rs", "fn main() {}\n");
    ws.attach_mount(app.path(), "app").unwrap();

    // A mount edit must never appear as root content, nor a root edit as
    // the mount's.
    fs::write(root.path().join("app").join("inner.txt"), "inside\n").unwrap();
    fs::write(root.path().join("outer.txt"), "outside\n").unwrap();

    let diff = ws.diff_latest().unwrap();
    let addresses: Vec<&str> = diff
        .deltas
        .iter()
        .map(|delta| delta.address.as_str())
        .collect();
    assert_eq!(addresses, vec!["outer.txt", "app/inner.txt"]);
}

#[test]
fn mount_refusals_speak_by_name() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let folder = folder_with("a.txt", "a\n");
    ws.attach_mount(folder.path(), "app").unwrap();

    // Attaching over an existing mount refuses.
    let error = ws.attach_mount(folder.path(), "app").unwrap_err();
    assert!(matches!(error, Error::AlreadyAttached), "got: {error:?}");

    // A mount colliding with existing root content refuses.
    fs::write(root.path().join("notes.txt"), "content\n").unwrap();
    ws.journal(1).unwrap();
    let error = ws.attach_mount(folder.path(), "notes.txt").unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("collides")),
        "got: {error:?}"
    );

    // Names that escape or shadow internals refuse.
    for name in ["", ".", "..", "a/b", ".atelier", ".jj", ".git"] {
        let error = ws.attach_mount(folder.path(), name).unwrap_err();
        assert!(
            matches!(&error, Error::Config(message) if message.contains("mount name")),
            "{name:?} got: {error:?}"
        );
    }

    // The root import still attaches once, beside any mounts.
    let import = folder_with("b.txt", "b\n");
    ws.attach(import.path()).unwrap();
    let error = ws.attach(import.path()).unwrap_err();
    assert!(matches!(error, Error::AlreadyAttached), "got: {error:?}");
}
