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

use atelier_core::{
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
