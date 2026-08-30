//! Mounted sources: per-source engines and histories, git adoption,
//! boundary isolation, the fan-out landing with per-source parking, the
//! manifest, and bookmark motion on landing.
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_sdk::{Error, Workspace};

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
fn a_git_repo_source_is_adopted_with_its_history() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let repo = tempfile::tempdir().unwrap();
    let pre_attach = git_repo(repo.path());

    let source = ws.attach_mount(repo.path(), "sdk").unwrap();
    assert_eq!(source.kind.to_string(), "local-git");

    // The adopted history is the mount's history: both pre-attach commits
    // list beneath the adoption, authorship preserved.
    let sdk_ids: Vec<String> = ws
        .log(50)
        .unwrap()
        .into_iter()
        .filter(|e| e.source.as_deref() == Some("sdk"))
        .map(|e| e.snapshot.id)
        .collect();
    assert!(
        sdk_ids.contains(&pre_attach[0]) && sdk_ids.contains(&pre_attach[1]),
        "pre-attach commits missing from {sdk_ids:?}"
    );
    let upstream = ws
        .log(50)
        .unwrap()
        .into_iter()
        .find(|e| e.snapshot.id == pre_attach[0])
        .expect("the first pre-attach commit lists");
    assert_eq!(upstream.snapshot.actor, "upstream");

    // A new edit snapshots into the adopted line, and plain git sees the
    // settled history: every snapshot but the open working-copy commit.
    fs::write(
        root.path().join("sdk").join("lib.rs"),
        "pub fn lib() { work() }\n",
    )
    .unwrap();
    ws.journal(1).unwrap();
    let git_log = std::process::Command::new("git")
        .args(["log", "--format=%H"])
        .current_dir(root.path().join("sdk"))
        .output()
        .expect("run git log");
    assert!(git_log.status.success());
    let seen = String::from_utf8(git_log.stdout).expect("git log is utf-8");
    assert!(
        seen.contains(&pre_attach[0]) && seen.contains(&pre_attach[1]),
        "git log lost the adopted history: {seen}"
    );

    // The mount stays a repo plain git pushes (story 14, per project).
    let bare = tempfile::tempdir().unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q", "--bare", "."])
        .current_dir(bare.path())
        .output()
        .expect("init bare remote");
    assert!(init.status.success());
    let push = std::process::Command::new("git")
        .args([
            "push",
            "-q",
            bare.path().to_str().expect("utf-8 path"),
            "master",
        ])
        .current_dir(root.path().join("sdk"))
        .output()
        .expect("push from the mount");
    assert!(push.status.success(), "push failed: {push:?}");
}

#[test]
fn an_lfs_git_source_refuses_at_attach() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    fs::write(
        repo.path().join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();

    let error = ws.attach_mount(repo.path(), "sdk").unwrap_err();
    assert!(
        matches!(error, Error::LfsSourceUnsupported),
        "got: {error:?}"
    );
}

#[test]
fn a_session_spans_every_source() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let app = folder_with("main.rs", "fn main() {}\n");
    let docs = folder_with("guide.md", "# Guide\n");
    ws.attach_mount(app.path(), "app").unwrap();
    ws.attach_mount(docs.path(), "docs").unwrap();

    let actor = atelier_sdk::Actor {
        name: "scribe".to_owned(),
        kind: atelier_sdk::ActorKind::Agent,
    };
    let instruction = atelier_sdk::Instruction {
        summary: "touch two projects".to_owned(),
        run_ref: None,
        verbatim: None,
    };
    let session = ws.open_session(&actor, &instruction).unwrap();

    // One change per source, root first then mounts by name, every id
    // its own.
    let sources: Vec<Option<&str>> = session
        .changes
        .iter()
        .map(|change| change.source.as_deref())
        .collect();
    assert_eq!(sources, vec![None, Some("app"), Some("docs")]);
    let ids: BTreeSet<&str> = session
        .changes
        .iter()
        .map(|change| change.change_id.as_str())
        .collect();
    assert_eq!(ids.len(), 3, "change ids collided: {:?}", session.changes);

    // Mount-scoped paths land in the mount's working copy; the mount's
    // shared line never sees session work.
    ws.session_write(session.id, "app/main.rs", "fn main() { run() }\n")
        .unwrap();
    ws.session_write(session.id, "plan.md", "# The plan\n")
        .unwrap();
    assert_eq!(
        fs::read_to_string(session.working_copy.join("app").join("main.rs")).unwrap(),
        "fn main() { run() }\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("app").join("main.rs")).unwrap(),
        "fn main() {}\n"
    );
    let read = ws.session_read(session.id, "app/main.rs", 0, None).unwrap();
    assert_eq!(read.content, "fn main() { run() }\n");

    // The session diff spans the touched sources, scoped; the untouched
    // docs source contributes nothing.
    let diff = ws.session_diff(session.id).unwrap();
    let addresses: Vec<&str> = diff
        .deltas
        .iter()
        .map(|delta| delta.address.as_str())
        .collect();
    assert_eq!(addresses, vec!["plan.md", "app/main.rs"]);

    // One request fans out: the root and the touched mount land, the
    // untouched docs source is never touched — no lease, no landing.
    let outcome = ws.land(session.id).unwrap();
    let atelier_sdk::GateOutcome::Landed { landings } = outcome else {
        panic!("the fan-out must land both touched sources, got {outcome:?}");
    };
    let landed: Vec<Option<&str>> = landings
        .iter()
        .map(|landing| landing.source.as_deref())
        .collect();
    assert_eq!(landed, vec![None, Some("app")]);
    // Both shared lines materialized their changes.
    assert_eq!(
        fs::read_to_string(root.path().join("plan.md")).unwrap(),
        "# The plan\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("app").join("main.rs")).unwrap(),
        "fn main() { run() }\n"
    );
    // One land act per source under one session; the mounted one names
    // its mount.
    let entries = ws.journal(50).unwrap();
    let land_refs: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.act == atelier_sdk::Act::Land)
        .filter_map(|entry| entry.reference.as_deref())
        .collect();
    assert_eq!(land_refs.len(), 2, "journal: {entries:#?}");
    assert!(
        land_refs.iter().any(|r| r.starts_with("app r1 ")),
        "no mounted land act: {land_refs:?}"
    );
    assert!(
        land_refs.iter().any(|r| r.starts_with("r1 ")),
        "no root land act: {land_refs:?}"
    );
    assert_eq!(
        ws.session(session.id).unwrap().state,
        atelier_sdk::SessionState::Landed
    );
}

#[test]
fn a_parked_mount_leaves_the_landed_sources_standing() {
    // The partial-landing story, end to end: A lands, B parks on a
    // conflict, the session stays open; resolving B re-lands it — and
    // the re-apply never repeats A's landing.
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let a = folder_with("a.txt", "a\n");
    let b = folder_with("b.txt", "b\n");
    ws.attach_mount(a.path(), "aa").unwrap();
    ws.attach_mount(b.path(), "bb").unwrap();

    let actor = atelier_sdk::Actor {
        name: "scribe".to_owned(),
        kind: atelier_sdk::ActorKind::Agent,
    };
    let instruction = atelier_sdk::Instruction {
        summary: "land across two projects".to_owned(),
        run_ref: None,
        verbatim: None,
    };
    let session = ws.open_session(&actor, &instruction).unwrap();
    ws.session_write(session.id, "aa/a.txt", "a from the session\n")
        .unwrap();
    ws.session_write(session.id, "bb/b.txt", "b from the session\n")
        .unwrap();

    // The bb line moves with a conflicting edit before the landing.
    fs::write(root.path().join("bb").join("b.txt"), "b from a human\n").unwrap();
    ws.journal(1).unwrap();

    let outcome = ws.land(session.id).unwrap();
    let atelier_sdk::GateOutcome::Parked {
        request,
        landings,
        parked,
    } = outcome
    else {
        panic!("the conflicting mount must park, got {outcome:?}");
    };
    assert_eq!(parked, vec![Some("bb".to_owned())]);
    let landed: Vec<Option<&str>> = landings
        .iter()
        .map(|landing| landing.source.as_deref())
        .collect();
    assert_eq!(landed, vec![None, Some("aa")]);
    // What landed stands; the parked line never moved.
    assert_eq!(
        fs::read_to_string(root.path().join("aa").join("a.txt")).unwrap(),
        "a from the session\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("bb").join("b.txt")).unwrap(),
        "b from a human\n"
    );
    assert_eq!(
        ws.session(session.id).unwrap().state,
        atelier_sdk::SessionState::Open
    );

    // The landed line's bookmark moved; the parked line has none to move.
    let rev_parse = |dir: &std::path::Path| {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "refs/heads/atelier"])
            .current_dir(dir)
            .output()
            .expect("run git");
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    };
    let aa_landing = landings[1].snapshot.clone();
    assert_eq!(rev_parse(&root.path().join("aa")), Some(aa_landing.clone()));
    assert_eq!(rev_parse(&root.path().join("bb")), None);

    // Resolve bb in the session: concede the contested line to the human
    // (revert the session's b.txt edit to its base) and carry the work as
    // a fresh file. The new snapshot re-opens the gate; the retry lands
    // only what remains.
    ws.session_write(session.id, "bb/b.txt", "b\n").unwrap();
    ws.session_write(session.id, "bb/c.txt", "the resolution\n")
        .unwrap();
    let outcome = ws.approve(request.id, &actor).unwrap();
    let atelier_sdk::GateOutcome::Landed { landings } = outcome else {
        panic!("the resolved retry must land, got {outcome:?}");
    };
    let landed: Vec<Option<&str>> = landings
        .iter()
        .map(|landing| landing.source.as_deref())
        .collect();
    assert_eq!(landed, vec![None, Some("aa"), Some("bb")]);
    assert_eq!(
        fs::read_to_string(root.path().join("bb").join("b.txt")).unwrap(),
        "b from a human\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("bb").join("c.txt")).unwrap(),
        "the resolution\n"
    );
    assert_eq!(
        ws.session(session.id).unwrap().state,
        atelier_sdk::SessionState::Landed
    );
    // aa landed exactly once: one land act names it across both applies.
    let entries = ws.journal(100).unwrap();
    let aa_lands = entries
        .iter()
        .filter(|entry| entry.act == atelier_sdk::Act::Land)
        .filter(|entry| {
            entry
                .reference
                .as_deref()
                .is_some_and(|r| r.starts_with("aa "))
        })
        .count();
    assert_eq!(aa_lands, 1);
}

#[test]
fn watch_routes_external_edits_into_the_owning_history() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let app = folder_with("main.rs", "fn main() {}\n");
    ws.attach_mount(app.path(), "app").unwrap();
    let app_head_before = ws
        .log(1)
        .unwrap()
        .into_iter()
        .find(|e| e.source.as_deref() == Some("app"))
        .expect("app has a history")
        .snapshot
        .id;

    let stop = atelier_sdk::WatchStop::new();
    let loop_stop = stop.clone();
    let (tx, events) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        ws.watch(
            std::time::Duration::from_millis(100),
            |event| {
                let _ = tx.send(event.clone());
            },
            &loop_stop,
        )
        .expect("watch loop runs until stopped");
        ws
    });
    assert_eq!(
        events
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the watcher arms"),
        atelier_sdk::WatchEvent::Started
    );

    fs::write(
        root.path().join("app").join("main.rs"),
        "fn main() { run() }\n",
    )
    .unwrap();
    let event = events
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the mount edit snapshots");
    assert!(
        matches!(event, atelier_sdk::WatchEvent::Snapshotted { .. }),
        "got: {event:?}"
    );
    stop.stop();
    let mut ws = handle.join().expect("watch thread joins");

    // The snapshot landed in the mount's history — not the root's — and
    // journaled with its mount.
    let app_head_after = ws
        .log(1)
        .unwrap()
        .into_iter()
        .find(|e| e.source.as_deref() == Some("app"))
        .expect("app has a history")
        .snapshot
        .id;
    assert_ne!(app_head_before, app_head_after);
    let entries = ws.journal(20).unwrap();
    assert!(
        entries.iter().any(|entry| {
            entry.act == atelier_sdk::Act::Snapshot
                && entry
                    .reference
                    .as_deref()
                    .is_some_and(|r| r == format!("app {app_head_after}"))
        }),
        "no mounted snapshot act: {entries:#?}"
    );
}

#[test]
fn the_manifest_orients_an_arriving_actor() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let app = tempfile::tempdir().unwrap();
    fs::write(app.path().join("main.rs"), "fn main() {}\n").unwrap();
    let source = ws.attach_mount(app.path(), "app").unwrap();
    let actor = atelier_sdk::Actor {
        name: "scribe".to_owned(),
        kind: atelier_sdk::ActorKind::Agent,
    };
    let instruction = atelier_sdk::Instruction {
        summary: "read the room".to_owned(),
        run_ref: None,
        verbatim: None,
    };
    let session = ws.open_session(&actor, &instruction).unwrap();

    let manifest = ws.manifest().unwrap();
    let logs = ws.log(50).unwrap();
    let root_head = logs
        .iter()
        .find(|entry| entry.source.is_none())
        .unwrap()
        .snapshot
        .id
        .clone();
    let app_head = logs
        .iter()
        .find(|entry| entry.source.as_deref() == Some("app"))
        .unwrap()
        .snapshot
        .id
        .clone();
    assert_eq!(
        manifest,
        format!(
            "workspace: {name}\n\
             schema: 1\n\
             \n\
             sources:\n\
             \x20 app  local-folder  {path}  two-way\n\
             \n\
             discipline:\n\
             \x20 approvals: 1  self-approval: allowed  snapshots dismiss approvals: yes\n\
             \x20 instructions: summary\n\
             \n\
             state:\n\
             \x20 head: {root_head}\n\
             \x20 head app: {app_head}\n\
             \x20 open sessions: {session_id}\n\
             \x20 live requests: none\n\
             \n\
             the loop:\n\
             \x20 open_session -> write -> diff -> land (or request_land + approve)\n\
             \x20 mount-scoped paths address sources; editing never takes the landing lease",
            name = root.path().file_name().unwrap().to_str().unwrap(),
            path = source.path.display(),
            session_id = session.id,
        )
    );
}

#[test]
fn a_landing_moves_the_branch_a_plain_push_carries() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let repo = tempfile::tempdir().unwrap();
    let pre_attach = git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();

    let git = |dir: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_owned()
    };
    let sdk = root.path().join("sdk");

    let actor = atelier_sdk::Actor {
        name: "scribe".to_owned(),
        kind: atelier_sdk::ActorKind::Agent,
    };
    let instruction = atelier_sdk::Instruction {
        summary: "ship the landed line".to_owned(),
        run_ref: None,
        verbatim: None,
    };
    let session = ws.open_session(&actor, &instruction).unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { push() }\n")
        .unwrap();

    // A session snapshot moves no branch: the adopted branch still names
    // the pre-attach tip.
    assert_eq!(
        git(&sdk, &["rev-parse", "refs/heads/master"]),
        pre_attach[1]
    );

    let outcome = ws.land(session.id).unwrap();
    let atelier_sdk::GateOutcome::Landed { landings } = outcome else {
        panic!("the land must land, got {outcome:?}");
    };
    let root_landing = &landings[0];
    let sdk_landing = &landings[1];
    assert_eq!(sdk_landing.source.as_deref(), Some("sdk"));

    // The adopted branch moved to the landed snapshot; the root landed on
    // the fallback bookmark.
    assert_eq!(
        git(&sdk, &["rev-parse", "refs/heads/master"]),
        sdk_landing.snapshot
    );
    assert_eq!(
        git(root.path(), &["rev-parse", "refs/heads/atelier"]),
        root_landing.snapshot
    );

    // Plain git push from the mount publishes the shared line.
    let bare = tempfile::tempdir().unwrap();
    git(bare.path(), &["init", "-q", "--bare", "-b", "master", "."]);
    git(
        &sdk,
        &["push", "-q", bare.path().to_str().unwrap(), "master"],
    );
    assert_eq!(
        git(bare.path(), &["rev-parse", "refs/heads/master"]),
        sdk_landing.snapshot
    );
}

#[test]
fn every_commit_atelier_writes_into_a_branch_carries_a_message() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();

    let git = |dir: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_owned()
    };
    let sdk = root.path().join("sdk");

    // An external edit becomes a stack snapshot beneath the landing.
    fs::write(sdk.join("lib.rs"), "pub fn lib() { observe() }\n").unwrap();
    ws.journal(1).unwrap();

    // A landed change carries the session's instruction summary as its
    // commit message, with the session as a trailer — on the adopted
    // branch and on the root's fallback bookmark alike.
    let actor = atelier_sdk::Actor {
        name: "scribe".to_owned(),
        kind: atelier_sdk::ActorKind::Agent,
    };
    let instruction = atelier_sdk::Instruction {
        summary: "wire the retry path".to_owned(),
        run_ref: None,
        verbatim: None,
    };
    let session = ws.open_session(&actor, &instruction).unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { retry() }\n")
        .unwrap();
    ws.session_write(session.id, "plan.md", "# retry\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    let atelier_sdk::GateOutcome::Landed { .. } = outcome else {
        panic!("the land must land, got {outcome:?}");
    };

    // The pushed branch reads whole: the landed summary, then the
    // snapshot and adoption continuations naming themselves, then the
    // adopted history — no blank subjects anywhere.
    assert_eq!(
        git(&sdk, &["log", "--format=%s", "refs/heads/master"]),
        "wire the retry path\nsnapshot\nadopt\nsecond pre-attach commit\nthe pre-attach commit"
    );

    let message = "wire the retry path\n\nAtelier-Session: s1";
    assert_eq!(
        git(&sdk, &["log", "-1", "--format=%B", "refs/heads/master"]),
        message
    );
    assert_eq!(
        git(
            root.path(),
            &["log", "-1", "--format=%B", "refs/heads/atelier"]
        ),
        message
    );
}
