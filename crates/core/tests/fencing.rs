//! Fenced landing leases (ADR-0014): a holder that stalls past its TTL
//! while a rival claims the same point cannot publish — a superseded
//! landing or undo refuses by name and a rerun completes what remains,
//! while a superseded fold skips like a held point. Each race stalls
//! the first holder with the hold seam, shrinks the TTL, and lets a
//! second workspace handle supersede it mid-stall.

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use atelier_sdk::{Act, Actor, ActorKind, Error, GateOutcome, Instruction, Workspace};

/// Serialize tests: they all set process-wide environment variables.
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

/// The race clock: the first holder stalls for `HOLD_MS` inside its
/// lease while the rival claims after `SUPERSEDE_AT_MS`, well past the
/// shrunken `TTL_MS`; margins absorb scheduler jitter.
const TTL_MS: u64 = 200;
const SUPERSEDE_AT_MS: u64 = 600;
const HOLD_MS: u64 = 1500;

/// Arms the stall-and-supersede seams; dropping it disarms them even
/// when an assertion panics mid-test.
struct RaceSeams;

impl RaceSeams {
    #[expect(unsafe_code, reason = "set_var arms the race seams under env_lock")]
    fn arm() -> Self {
        // SAFETY: as in `set_actor`; guarded by `env_lock()`.
        unsafe {
            std::env::set_var("ATELIER_LANDING_LEASE_TTL_MS", TTL_MS.to_string());
            std::env::set_var("ATELIER_LAND_HOLD_MS", HOLD_MS.to_string());
        }
        Self
    }
}

impl Drop for RaceSeams {
    #[expect(
        unsafe_code,
        reason = "remove_var disarms the race seams under env_lock"
    )]
    fn drop(&mut self) {
        // SAFETY: as in `set_actor`; guarded by `env_lock()`.
        unsafe {
            std::env::remove_var("ATELIER_LANDING_LEASE_TTL_MS");
            std::env::remove_var("ATELIER_LAND_HOLD_MS");
        }
    }
}

fn agent(name: &str) -> Actor {
    Actor {
        name: name.to_owned(),
        kind: ActorKind::Agent,
    }
}

fn instruction(summary: &str) -> Instruction {
    Instruction {
        summary: summary.to_owned(),
        run_ref: None,
        verbatim: None,
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
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
}

#[test]
fn a_superseded_landing_refuses_by_name_and_a_rerun_completes() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws_a = Workspace::init(root.path()).unwrap();
    let mut ws_b = Workspace::open(root.path()).unwrap();

    let session_a = ws_a
        .open_session(&agent("one"), &instruction("land file a"))
        .unwrap();
    ws_a.session_write(session_a.id, "a.txt", "a\n").unwrap();
    let session_b = ws_b
        .open_session(&agent("two"), &instruction("land file b"))
        .unwrap();
    ws_b.session_write(session_b.id, "b.txt", "b\n").unwrap();

    let seams = RaceSeams::arm();
    let stalled = std::thread::spawn(move || {
        let outcome = ws_a.land(session_a.id);
        (ws_a, outcome)
    });
    std::thread::sleep(Duration::from_millis(SUPERSEDE_AT_MS));
    // The stalled holder's lease expired; this claim supersedes its
    // tenancy and lands.
    let outcome = ws_b.land(session_b.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );
    let (mut ws_a, stalled_outcome) = stalled.join().expect("join the stalled holder");
    let Err(error) = stalled_outcome else {
        panic!("the superseded holder must not publish");
    };
    assert!(
        matches!(&error, Error::LeaseSuperseded { point } if point == "landing"),
        "got: {error:?}"
    );
    drop(seams);

    // Nothing of the superseded attempt published: the rival's landing
    // stands alone, and the rerun completes what remains on top of it.
    assert!(!root.path().join("a.txt").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "b\n"
    );
    let outcome = ws_a.land(session_a.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "a\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "b\n"
    );
}

#[test]
fn a_superseded_undo_refuses_by_name_and_the_line_steps_once() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws_a = Workspace::init(root.path()).unwrap();
    let mut ws_b = Workspace::open(root.path()).unwrap();

    let session = ws_a
        .open_session(&agent("one"), &instruction("land the note"))
        .unwrap();
    ws_a.session_write(session.id, "note.txt", "landed\n")
        .unwrap();
    let outcome = ws_a.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );
    let request = ws_a.landing_requests().unwrap()[0].id;

    let seams = RaceSeams::arm();
    let stalled = std::thread::spawn(move || {
        let outcome = ws_a.undo(request);
        (ws_a, outcome)
    });
    std::thread::sleep(Duration::from_millis(SUPERSEDE_AT_MS));
    ws_b.undo(request).unwrap();
    let (mut ws_a, stalled_outcome) = stalled.join().expect("join the stalled holder");
    let Err(error) = stalled_outcome else {
        panic!("the superseded holder must not publish");
    };
    assert!(
        matches!(&error, Error::LeaseSuperseded { point } if point == "landing"),
        "got: {error:?}"
    );
    drop(seams);

    // The line stepped exactly once: the landed file is gone, and one
    // undo act is journaled — a second step would have republished the
    // undone head.
    assert!(!root.path().join("note.txt").exists());
    let undos = ws_a
        .journal(50)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.act == Act::Undo)
        .count();
    assert_eq!(undos, 1);
}

#[test]
fn a_superseded_fold_skips_and_the_line_folds_once() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws_a = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "master", "."]);
    fs::write(repo.path().join("lib.rs"), "pub fn lib() {}\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "the pre-attach commit"]);
    ws_a.attach_mount(repo.path(), "sdk").unwrap();
    let mut ws_b = Workspace::open(root.path()).unwrap();
    let sdk = root.path().join("sdk");

    // Stage the drift both handles will race to fold: a message rewrite
    // of the branch tip.
    let tip = git(&sdk, &["rev-parse", "refs/heads/master"]);
    let tree = git(&sdk, &["rev-parse", "refs/heads/master^{tree}"]);
    let amended = git(&sdk, &["commit-tree", &tree, "-m", "human message"]);
    git(&sdk, &["update-ref", "refs/heads/master", &amended, &tip]);

    let seams = RaceSeams::arm();
    let stalled = std::thread::spawn(move || {
        let outcome = ws_a.status();
        (ws_a, outcome)
    });
    std::thread::sleep(Duration::from_millis(SUPERSEDE_AT_MS));
    ws_b.status().unwrap();
    let (mut ws_a, stalled_outcome) = stalled.join().expect("join the stalled holder");
    // A superseded fold skips exactly like a held point: the rival
    // folded the line, and the stalled operation still answers.
    stalled_outcome.expect("the superseded fold must skip, not fail");
    drop(seams);

    assert_eq!(git(&sdk, &["rev-parse", "refs/heads/master"]), amended);
    let pulls = ws_a
        .journal(50)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.act == Act::Pull)
        .count();
    assert_eq!(pulls, 1);
}
