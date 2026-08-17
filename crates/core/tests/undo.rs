//! Undo (ADR-0011): a landed request steps back off every line it landed,
//! the gate re-opens for a new decision, origins re-mirror — and every
//! non-undoable thing refuses by name.
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_sdk::{
    Act, Actor, ActorKind, Error, GateOutcome, Instruction, RequestId, RequestState, SessionState,
    Workspace,
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
        summary: "work to reconsider".to_owned(),
        run_ref: None,
        verbatim: None,
    }
}

fn head_of(ws: &mut Workspace, source: Option<&str>) -> String {
    ws.log(50)
        .expect("read the log")
        .into_iter()
        .find(|entry| entry.source.as_deref() == source)
        .expect("the source has a head")
        .snapshot
        .id
}

fn expect_landed(outcome: &GateOutcome) {
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "the land must land, got {outcome:?}"
    );
}

#[test]
fn undoing_a_fanned_out_landing_restores_every_line() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let root_origin = tempfile::tempdir().unwrap();
    fs::write(root_origin.path().join("notes.txt"), "the note\n").unwrap();
    ws.attach(root_origin.path()).unwrap();
    let docs_origin = tempfile::tempdir().unwrap();
    fs::write(docs_origin.path().join("guide.md"), "# Guide\n").unwrap();
    ws.attach_mount(docs_origin.path(), "docs").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "the revised note\n")
        .unwrap();
    ws.session_write(session.id, "docs/guide.md", "# Revised guide\n")
        .unwrap();
    let root_before = head_of(&mut ws, None);
    let docs_before = head_of(&mut ws, Some("docs"));

    let outcome = ws.land(session.id).unwrap();
    expect_landed(&outcome);
    let id: RequestId = "r1".parse().unwrap();
    assert_eq!(
        fs::read_to_string(root_origin.path().join("notes.txt")).unwrap(),
        "the revised note\n"
    );

    // The undo steps every line back, mounts before the root.
    let restores = ws.undo(id).unwrap();
    let sources: Vec<Option<&str>> = restores
        .iter()
        .map(|restore| restore.source.as_deref())
        .collect();
    assert_eq!(sources, vec![Some("docs"), None]);
    assert_eq!(restores[0].head, docs_before);
    assert_eq!(restores[1].head, root_before);
    assert_eq!(head_of(&mut ws, None), root_before);
    assert_eq!(head_of(&mut ws, Some("docs")), docs_before);

    // The shared line's content returned, and the origins re-mirrored.
    assert_eq!(
        fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "the note\n"
    );
    assert_eq!(
        fs::read_to_string(root_origin.path().join("notes.txt")).unwrap(),
        "the note\n"
    );
    assert_eq!(
        fs::read_to_string(docs_origin.path().join("guide.md")).unwrap(),
        "# Guide\n"
    );

    // The gate re-opened: the request is open with no live approvals, the
    // session is open, and the journal names each stepped line.
    let request = ws
        .landing_requests()
        .unwrap()
        .into_iter()
        .find(|request| request.id == id)
        .expect("the request lists");
    assert_eq!(request.state, RequestState::Open);
    assert!(request.approvals.is_empty(), "got: {:?}", request.approvals);
    assert_eq!(ws.session(session.id).unwrap().state, SessionState::Open);
    let undo_refs: Vec<String> = ws
        .journal(50)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::Undo)
        .map(|entry| entry.reference.unwrap_or_default())
        .collect();
    assert_eq!(
        undo_refs,
        vec![
            format!("{id} {root_before}"),
            format!("docs {id} {docs_before}")
        ]
    );

    // The change survived the undo: approving again lands it again.
    let outcome = ws.approve(id, &actor()).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(root_origin.path().join("notes.txt")).unwrap(),
        "the revised note\n"
    );
}

#[test]
fn only_a_landed_request_undoes() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "a.txt", "a\n").unwrap();
    let request = ws.request_land(session.id).unwrap();

    let error = ws.undo(request.id).unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("only a landed request")),
        "got: {error:?}"
    );

    let error = ws.undo("r9".parse().unwrap()).unwrap_err();
    assert!(
        matches!(&error, Error::RequestNotFound(_)),
        "got: {error:?}"
    );
}

#[test]
fn undo_composes_backward_through_the_stack() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("a.txt"), "base\n").unwrap();

    let first = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(first.id, "a.txt", "first\n").unwrap();
    let base = head_of(&mut ws, None);
    expect_landed(&ws.land(first.id).unwrap());
    let first_landed = head_of(&mut ws, None);

    let second = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(second.id, "a.txt", "second\n").unwrap();
    let outcome = ws.land(second.id).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }));

    // r1 is buried under r2: the line moved past it, by name.
    let r1: RequestId = "r1".parse().unwrap();
    let r2: RequestId = "r2".parse().unwrap();
    let error = ws.undo(r1).unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("moved past r1")),
        "got: {error:?}"
    );

    // Undo composes backward: r2 first, then r1, one landing at a time.
    ws.undo(r2).unwrap();
    assert_eq!(head_of(&mut ws, None), first_landed);
    assert_eq!(
        fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "first\n"
    );
    ws.undo(r1).unwrap();
    assert_eq!(head_of(&mut ws, None), base);
    assert_eq!(
        fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn undo_unwinds_a_landing_recorded_across_two_applies() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let aa = tempfile::tempdir().unwrap();
    fs::write(aa.path().join("a.txt"), "a\n").unwrap();
    ws.attach_mount(aa.path(), "aa").unwrap();
    let bb = tempfile::tempdir().unwrap();
    fs::write(bb.path().join("b.txt"), "b\n").unwrap();
    ws.attach_mount(bb.path(), "bb").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "aa/a.txt", "a from the session\n")
        .unwrap();
    ws.session_write(session.id, "bb/b.txt", "b from the session\n")
        .unwrap();

    // A human moves bb's shared line; the first apply lands root and aa
    // and parks bb.
    fs::write(root.path().join("bb").join("b.txt"), "b from a human\n").unwrap();
    let bb_human_head = head_of(&mut ws, Some("bb"));
    let root_before = head_of(&mut ws, None);
    let aa_before = head_of(&mut ws, Some("aa"));
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Parked { .. }),
        "got: {outcome:?}"
    );

    // The resolution concedes the contested line and carries the work as
    // a fresh file; the retry lands what remains - bb, on the second
    // apply, so the request's landings span two applies.
    ws.session_write(session.id, "bb/b.txt", "b from a human\n")
        .unwrap();
    ws.session_write(session.id, "bb/c.txt", "the resolution\n")
        .unwrap();
    let id: RequestId = "r1".parse().unwrap();
    let outcome = ws.approve(id, &actor()).unwrap();
    expect_landed(&outcome);

    // The undo steps every recorded line back - bb to the human's head,
    // aa and the root to their pre-landing heads.
    let restores = ws.undo(id).unwrap();
    let sources: Vec<Option<&str>> = restores
        .iter()
        .map(|restore| restore.source.as_deref())
        .collect();
    assert_eq!(sources, vec![Some("bb"), Some("aa"), None]);
    assert_eq!(head_of(&mut ws, Some("bb")), bb_human_head);
    assert_eq!(head_of(&mut ws, Some("aa")), aa_before);
    assert_eq!(head_of(&mut ws, None), root_before);
    assert_eq!(
        fs::read_to_string(root.path().join("bb").join("b.txt")).unwrap(),
        "b from a human\n"
    );
    assert!(!root.path().join("bb").join("c.txt").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("aa").join("a.txt")).unwrap(),
        "a\n"
    );

    // The whole request re-lands anew: all three lines, fresh records.
    let outcome = ws.approve(id, &actor()).unwrap();
    let GateOutcome::Landed { landings } = outcome else {
        panic!("the re-approval must land, got {outcome:?}");
    };
    assert_eq!(landings.len(), 3);
    assert_eq!(
        fs::read_to_string(root.path().join("bb").join("c.txt")).unwrap(),
        "the resolution\n"
    );
}

#[test]
fn outstanding_edits_block_an_undo_and_lose_nothing() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("notes.txt"), "base\n").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "landed\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    expect_landed(&outcome);
    let landed_head = head_of(&mut ws, None);

    // A human edits the shared line after the landing. The undo's own
    // auto-snapshot captures the edit first, so the line has moved past
    // the landing - the undo refuses and the edit survives, versioned.
    fs::write(root.path().join("notes.txt"), "edited after landing\n").unwrap();
    let id: RequestId = "r1".parse().unwrap();
    let error = ws.undo(id).unwrap_err();
    assert!(
        matches!(&error, Error::Config(message) if message.contains("moved past r1")),
        "got: {error:?}"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "edited after landing\n"
    );
    let head = head_of(&mut ws, None);
    assert_ne!(head, landed_head);
    // The request stayed landed: nothing half-moved.
    let request = ws
        .landing_requests()
        .unwrap()
        .into_iter()
        .find(|request| request.id == id)
        .expect("the request lists");
    assert_eq!(request.state, RequestState::Landed);
}

#[test]
fn a_dirty_origin_parks_the_undo_mirror_and_the_undo_stands() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let origin = tempfile::tempdir().unwrap();
    fs::write(origin.path().join("doc.md"), "v1\n").unwrap();
    ws.attach_mount(origin.path(), "docs").unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "docs/doc.md", "v2\n").unwrap();
    let docs_before = head_of(&mut ws, Some("docs"));
    expect_landed(&ws.land(session.id).unwrap());
    assert_eq!(
        fs::read_to_string(origin.path().join("doc.md")).unwrap(),
        "v2\n"
    );

    // The human keeps working in the origin after the landing's sync.
    fs::write(origin.path().join("doc.md"), "the human's v3\n").unwrap();

    // The undo steps the line back; the re-mirror parks - the origin is
    // never overwritten, and the journal says so.
    let id: RequestId = "r1".parse().unwrap();
    let restores = ws.undo(id).unwrap();
    assert_eq!(restores[0].head, docs_before);
    assert_eq!(head_of(&mut ws, Some("docs")), docs_before);
    assert_eq!(
        fs::read_to_string(origin.path().join("doc.md")).unwrap(),
        "the human's v3\n"
    );
    let parked: Vec<String> = ws
        .journal(50)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::SyncParked)
        .map(|entry| entry.reference.unwrap_or_default())
        .collect();
    assert_eq!(parked, vec![format!("docs {docs_before} origin changed")]);
}

#[test]
fn an_open_request_on_the_same_line_survives_an_undo_beneath_it() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("a.txt"), "base\n").unwrap();

    let first = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(first.id, "a.txt", "first\n").unwrap();
    let base = head_of(&mut ws, None);
    expect_landed(&ws.land(first.id).unwrap());

    // A second session opens its request but does not land.
    let second = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(second.id, "b.txt", "second's work\n")
        .unwrap();
    let pending = ws.request_land(second.id).unwrap();
    assert_eq!(pending.state, RequestState::Open);

    // Undoing r1 beneath it does not disturb the open gate.
    let r1: RequestId = "r1".parse().unwrap();
    ws.undo(r1).unwrap();
    assert_eq!(head_of(&mut ws, None), base);
    let still_open = ws
        .landing_requests()
        .unwrap()
        .into_iter()
        .find(|request| request.id == pending.id)
        .expect("the pending request lists");
    assert_eq!(still_open.state, RequestState::Open);

    // The pending request lands onto the restored head.
    let outcome = ws.approve(pending.id, &actor()).unwrap();
    expect_landed(&outcome);
    assert_eq!(
        fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "second's work\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn an_abandoned_session_keeps_the_undone_request_closed() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "a.txt", "work\n").unwrap();
    expect_landed(&ws.land(session.id).unwrap());

    let id: RequestId = "r1".parse().unwrap();
    ws.undo(id).unwrap();
    ws.abandon(session.id).unwrap();

    // The abandonment closed the re-opened gate for good.
    let error = ws.approve(id, &actor()).unwrap_err();
    assert!(
        matches!(&error, Error::RequestClosed { state, .. } if state == "abandoned"),
        "got: {error:?}"
    );
    assert!(!root.path().join("a.txt").exists());
}

#[test]
fn undo_steps_an_adopted_branch_back_for_plain_git() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();

    let repo = tempfile::tempdir().unwrap();
    let git = |dir: &Path, args: &[&str]| {
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
        String::from_utf8(output.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_owned()
    };
    git(repo.path(), &["init", "-q", "-b", "master", "."]);
    fs::write(repo.path().join("lib.rs"), "pub fn lib() {}\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "the pre-attach commit"]);
    let pre_attach = git(repo.path(), &["rev-parse", "HEAD"]);
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let sdk = root.path().join("sdk");

    let session = ws.open_session(&actor(), &instruction()).unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { work() }\n")
        .unwrap();
    let sdk_before = head_of(&mut ws, Some("sdk"));
    expect_landed(&ws.land(session.id).unwrap());
    let landed_head = head_of(&mut ws, Some("sdk"));
    assert_eq!(git(&sdk, &["rev-parse", "refs/heads/master"]), landed_head);

    // The undo steps the adopted branch back with the line, so a plain
    // push publishes the restored head - never the undone one.
    let id: RequestId = "r1".parse().unwrap();
    let restores = ws.undo(id).unwrap();
    assert_eq!(restores[0].head, sdk_before);
    assert_eq!(git(&sdk, &["rev-parse", "refs/heads/master"]), sdk_before);
    let bare = tempfile::tempdir().unwrap();
    git(bare.path(), &["init", "-q", "--bare", "-b", "master", "."]);
    git(
        &sdk,
        &["push", "-q", bare.path().to_str().unwrap(), "master"],
    );
    assert_eq!(
        git(bare.path(), &["rev-parse", "refs/heads/master"]),
        sdk_before
    );
    // The adopted history beneath the restored head is intact.
    assert_eq!(
        git(bare.path(), &["rev-parse", "refs/heads/master~1"]),
        pre_attach
    );
}
