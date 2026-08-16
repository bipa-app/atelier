//! Sessions and the landing gate: every legal transition lands and each
//! illegal one refuses by name — open, request, approve, reject, park,
//! abandon, and the races between them.
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use atelier_core::{
    Act, Actor, ActorKind, Error, GateOutcome, Instruction, RequestState, SessionState, Workspace,
};

/// Serialize tests: they all set the process-wide `ATELIER_CONFIG_HOME`.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
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

fn agent() -> Actor {
    Actor {
        name: "scribe".to_owned(),
        kind: ActorKind::Agent,
    }
}

fn human() -> Actor {
    Actor {
        name: "test-actor".to_owned(),
        kind: ActorKind::Human,
    }
}

fn instruction() -> Instruction {
    Instruction {
        summary: "redline the draft".to_owned(),
        run_ref: Some("harness:run/42".to_owned()),
        verbatim: Some("please redline the draft".to_owned()),
    }
}

#[test]
fn an_agent_session_lands_through_the_gate() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("notes.txt"), "shared draft\n").unwrap();

    let session = ws.open_session(&agent(), &instruction()).unwrap();
    assert_eq!(session.id.to_string(), "s1");
    assert_eq!(session.state, SessionState::Open);
    assert_eq!(session.actor, agent());
    assert!(session.working_copy.ends_with(".atelier/sessions/s1"));
    assert_eq!(
        fs::read_to_string(session.working_copy.join("notes.txt")).unwrap(),
        "shared draft\n"
    );

    // The session edits in isolation: the shared line must not see it.
    ws.session_write(session.id, "notes.txt", "agent draft\n")
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "shared draft\n"
    );

    let diff = ws.session_diff(session.id).unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "notes.txt");
    assert!(
        diff.deltas[0]
            .lines
            .iter()
            .any(|line| line.text == "agent draft")
    );

    let request = ws.request_land(session.id).unwrap();
    assert_eq!(request.id.to_string(), "r1");
    assert_eq!(request.state, RequestState::Open);
    // Asking again returns the request already holding the gate.
    assert_eq!(ws.request_land(session.id).unwrap().id, request.id);

    let outcome = ws.approve(request.id, &agent()).unwrap();
    let GateOutcome::Landed { landings } = outcome else {
        panic!("self-approval under the default policy must land, got {outcome:?}");
    };
    assert_eq!(landings.len(), 1);
    assert_eq!(landings[0].source, None);
    let snapshot = landings[0].snapshot.clone();

    // The shared line advanced to the landed snapshot, attributed to the
    // agent, and the root working copy materialized the change.
    let log = ws.log(5).unwrap();
    assert_eq!(log[0].snapshot.id, snapshot);
    assert_eq!(log[0].snapshot.actor, "scribe");
    assert_eq!(
        fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "agent draft\n"
    );

    assert_eq!(ws.request(request.id).unwrap().state, RequestState::Landed);
    assert_eq!(ws.session(session.id).unwrap().state, SessionState::Landed);

    // The journal groups the session's acts under it, with the
    // instruction's summary and run reference.
    let entries = ws.journal(50).unwrap();
    let opened = entries
        .iter()
        .find(|entry| entry.act == Act::SessionOpen)
        .expect("session_open is journaled");
    assert_eq!(opened.session.as_deref(), Some("s1"));
    assert_eq!(
        opened.instruction_summary.as_deref(),
        Some("redline the draft")
    );
    assert_eq!(opened.instruction_run_ref.as_deref(), Some("harness:run/42"));
    // The default policy keeps the summary, never the verbatim prompt.
    assert_eq!(opened.instruction_verbatim, None);
    for act in [Act::LandRequest, Act::Approve, Act::Land] {
        let entry = entries
            .iter()
            .find(|entry| entry.act == act)
            .unwrap_or_else(|| panic!("{act} is journaled"));
        assert_eq!(entry.session.as_deref(), Some("s1"));
    }

    // A landed session is closed for work.
    let refused = ws.session_write(session.id, "notes.txt", "more\n");
    assert!(matches!(refused, Err(Error::SessionClosed { .. })));
}

#[test]
fn landing_into_a_moved_conflicting_line_parks_the_request() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("notes.txt"), "first line\n").unwrap();

    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "agent version\n")
        .unwrap();

    // The shared line moves with a conflicting edit.
    fs::write(root.path().join("notes.txt"), "human version\n").unwrap();
    let head_before = ws.log(1).unwrap()[0].snapshot.id.clone();

    let request = ws.request_land(session.id).unwrap();
    let outcome = ws.approve(request.id, &agent()).unwrap();
    let GateOutcome::Parked {
        request: parked,
        landings,
        parked: parked_sources,
    } = outcome
    else {
        panic!("a conflicting apply must park, got {outcome:?}");
    };
    assert_eq!(parked.state, RequestState::Parked);
    // The root is the parked line; nothing landed under this request.
    assert_eq!(landings, Vec::new());
    assert_eq!(parked_sources, vec![None]);

    // The shared line did not move and never carries the conflict.
    assert_eq!(ws.log(1).unwrap()[0].snapshot.id, head_before);
    assert_eq!(
        fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "human version\n"
    );
    assert!(
        ws.journal(50)
            .unwrap()
            .iter()
            .any(|entry| entry.act == Act::LandParked)
    );

    // A parked request refuses approval; resolution is a new snapshot.
    let refused = ws.approve(request.id, &human());
    assert!(matches!(refused, Err(Error::RequestParked(_))));

    // Matching the shared line's content resolves the conflict; the new
    // snapshot re-opens the gate and the change lands.
    ws.session_write(session.id, "notes.txt", "human version\n")
        .unwrap();
    let reopened = ws.request_land(session.id).unwrap();
    assert_eq!(reopened.id, request.id);
    assert_eq!(reopened.state, RequestState::Open);
    let outcome = ws.approve(request.id, &agent()).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }));
}

#[test]
fn a_new_snapshot_after_approval_dismisses_it_and_reopens_the_gate() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(
        root.path().join(".atelier/config.toml"),
        "schema = 1\n\n[workspace]\nname = \"gated\"\n\n[landing]\napprovals = 2\n",
    )
    .unwrap();

    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "first draft\n")
        .unwrap();
    let request = ws.request_land(session.id).unwrap();

    let outcome = ws.approve(request.id, &human()).unwrap();
    let GateOutcome::Pending {
        request: pending,
        required,
    } = outcome
    else {
        panic!("one of two approvals must stay pending, got {outcome:?}");
    };
    assert_eq!((pending.approvals.len(), required), (1, 2));

    // A new snapshot on the change dismisses the approval.
    ws.session_write(session.id, "notes.txt", "second draft\n")
        .unwrap();
    let reopened = ws.request(request.id).unwrap();
    assert_eq!(reopened.state, RequestState::Open);
    assert_eq!(reopened.approvals.len(), 0);
    assert!(
        ws.journal(50)
            .unwrap()
            .iter()
            .any(|entry| entry.act == Act::ApprovalsDismissed)
    );

    // The gate starts over: two fresh approvals land the change.
    let outcome = ws.approve(request.id, &human()).unwrap();
    assert!(matches!(outcome, GateOutcome::Pending { .. }));
    let outcome = ws.approve(request.id, &agent()).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }));
}

#[test]
fn abandon_closes_the_session_and_its_request() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "draft\n")
        .unwrap();
    let request = ws.request_land(session.id).unwrap();

    let abandoned = ws.abandon(session.id).unwrap();
    assert_eq!(abandoned.state, SessionState::Abandoned);
    assert_eq!(
        ws.request(request.id).unwrap().state,
        RequestState::Abandoned
    );
    // The work stays on disk; the session is closed for new work.
    assert!(abandoned.working_copy.join("notes.txt").is_file());
    let refused = ws.session_write(session.id, "notes.txt", "more\n");
    assert!(matches!(refused, Err(Error::SessionClosed { .. })));
    assert!(
        ws.journal(50)
            .unwrap()
            .iter()
            .any(|entry| entry.act == Act::SessionAbandon)
    );
}

#[test]
fn a_rejected_request_closes_and_a_new_one_can_open() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "draft\n")
        .unwrap();
    let request = ws.request_land(session.id).unwrap();

    let rejected = ws
        .reject(request.id, &human(), Some("needs a second pass"))
        .unwrap();
    assert_eq!(rejected.state, RequestState::Rejected);
    let refused = ws.approve(request.id, &human());
    assert!(matches!(refused, Err(Error::RequestClosed { .. })));

    // The session stays open; asking to land opens a fresh request.
    let second = ws.request_land(session.id).unwrap();
    assert_ne!(second.id, request.id);
    assert_eq!(second.state, RequestState::Open);
}

#[test]
fn session_paths_never_leave_the_working_copy() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    let session = ws.open_session(&agent(), &instruction()).unwrap();

    let climb = ws.session_write(session.id, "../escape.txt", "out\n");
    assert!(matches!(climb, Err(Error::PathOutsideWorkingCopy(_))));
    let absolute = ws.session_read(session.id, "/etc/hosts", 0, None);
    assert!(matches!(absolute, Err(Error::PathOutsideWorkingCopy(_))));
}

#[test]
fn every_closed_gate_refuses_each_transition_by_name() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("notes.txt"), "shared\n").unwrap();

    // Landed: approve, reject, and a fresh approval all refuse by name.
    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "landed work\n")
        .unwrap();
    let request = ws.request_land(session.id).unwrap();
    let outcome = ws.approve(request.id, &agent()).unwrap();
    assert!(matches!(outcome, GateOutcome::Landed { .. }));
    for refused in [
        ws.approve(request.id, &human()).map(|_| ()).err(),
        ws.reject(request.id, &human(), None).map(|_| ()).err(),
    ] {
        let error = refused.expect("a landed gate is closed");
        assert!(
            matches!(&error, Error::RequestClosed { state, .. } if state == "landed"),
            "unexpected refusal: {error:?}"
        );
    }
    let error = ws
        .abandon(session.id)
        .expect_err("a landed session is closed");
    assert!(matches!(&error, Error::SessionClosed { state, .. } if state == "landed"));

    // Rejected: the gate stays closed to approval and re-rejection.
    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "rejected work\n")
        .unwrap();
    let request = ws.request_land(session.id).unwrap();
    ws.reject(request.id, &human(), Some("not yet")).unwrap();
    for refused in [
        ws.approve(request.id, &human()).map(|_| ()).err(),
        ws.reject(request.id, &human(), None).map(|_| ()).err(),
    ] {
        let error = refused.expect("a rejected gate is closed");
        assert!(
            matches!(&error, Error::RequestClosed { state, .. } if state == "rejected"),
            "unexpected refusal: {error:?}"
        );
    }

    // Abandoned: same story, session and request both closed.
    let request = ws.request_land(session.id).unwrap();
    ws.abandon(session.id).unwrap();
    let error = ws
        .approve(request.id, &human())
        .expect_err("an abandoned gate is closed");
    assert!(matches!(&error, Error::RequestClosed { state, .. } if state == "abandoned"));
    let error = ws
        .abandon(session.id)
        .expect_err("an abandoned session is closed");
    assert!(matches!(&error, Error::SessionClosed { state, .. } if state == "abandoned"));
}

#[expect(
    unsafe_code,
    reason = "set_var arms the land-hold test seam in a locked test"
)]
fn hold_applies(ms: &str) {
    // SAFETY: as in set_actor; guarded by `env_lock()`.
    unsafe {
        std::env::set_var("ATELIER_LAND_HOLD_MS", ms);
    }
}

#[expect(
    unsafe_code,
    reason = "remove_var disarms the land-hold test seam in a locked test"
)]
fn release_hold() {
    // SAFETY: as in set_actor; guarded by `env_lock()`.
    unsafe {
        std::env::remove_var("ATELIER_LAND_HOLD_MS");
    }
}

#[test]
fn a_stale_apply_cannot_overwrite_a_concurrent_abandonment() {
    // The apply holds the landing lease and sleeps (the test seam);
    // another handle abandons the session meanwhile. The engine landing
    // is history and journals, but the apply's request and session
    // transitions arrive stale — the abandonment must stand.
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    fs::write(root.path().join("notes.txt"), "shared\n").unwrap();

    let session = ws.open_session(&agent(), &instruction()).unwrap();
    ws.session_write(session.id, "notes.txt", "agent draft\n")
        .unwrap();
    let request = ws.request_land(session.id).unwrap();

    hold_applies("1500");
    let mut applier = Workspace::open(root.path()).unwrap();
    let apply = std::thread::spawn(move || applier.approve(request.id, &agent()));

    // Wait for the applier to pass the gate and enter its held apply.
    let mut ws_b = Workspace::open(root.path()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while ws_b.request(request.id).unwrap().state != RequestState::Approved {
        assert!(
            std::time::Instant::now() < deadline,
            "the applier never approved"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    ws_b.abandon(session.id).unwrap();

    let outcome = apply.join().expect("the applier thread joins").unwrap();
    release_hold();

    // The engine landed the approved tip — that is history and journaled —
    // but the store rows keep the abandonment: nothing was overwritten.
    let GateOutcome::Landed { landings } = outcome else {
        panic!("the held apply landed the approved tip, got {outcome:?}");
    };
    let snapshot = landings[0].snapshot.clone();
    assert_eq!(ws_b.log(2).unwrap()[0].snapshot.id, snapshot);
    assert_eq!(
        ws_b.request(request.id).unwrap().state,
        RequestState::Abandoned
    );
    assert_eq!(
        ws_b.session(session.id).unwrap().state,
        SessionState::Abandoned
    );
    let entries = ws_b.journal(50).unwrap();
    for act in [Act::Land, Act::SessionAbandon] {
        assert!(
            entries.iter().any(|entry| entry.act == act),
            "{act} must be journaled"
        );
    }
}
