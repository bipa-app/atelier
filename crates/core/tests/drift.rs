//! Out-of-band git operations on adopted mounts: a recommit, a branch
//! move, or a push folds into the line on the next workspace operation
//! (journaled as a pull), open sessions follow the fold, and a content
//! conflict refuses by name — the next landing always exports cleanly.

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_sdk::{Act, Actor, ActorKind, Error, GateOutcome, Instruction, Workspace};

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

/// A git repository fixture with two commits.
fn git_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "master", "."]);
    fs::write(dir.join("lib.rs"), "pub fn lib() {}\n").expect("write repo file");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "the pre-attach commit"]);
    fs::write(dir.join("README.md"), "readme\n").expect("write repo file");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "second pre-attach commit"]);
}

fn agent() -> Actor {
    Actor {
        name: "scribe".to_owned(),
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

#[test]
fn an_out_of_band_recommit_folds_and_the_next_land_succeeds() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let sdk = root.path().join("sdk");

    let session = ws
        .open_session(&agent(), &instruction("land the retry"))
        .unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { retry() }\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );

    // The out-of-band move the issue reports: a human rewrites the landed
    // commit's message on the branch, in the mount, with plain git
    // (plumbing spells what reset --soft plus commit spells: same tree,
    // same parent, new message, branch moved).
    let landed = git(&sdk, &["rev-parse", "refs/heads/master"]);
    let tree = git(&sdk, &["rev-parse", "refs/heads/master^{tree}"]);
    let parent = git(&sdk, &["rev-parse", "refs/heads/master^"]);
    let amended = git(
        &sdk,
        &["commit-tree", &tree, "-p", &parent, "-m", "human message"],
    );
    git(
        &sdk,
        &["update-ref", "refs/heads/master", &amended, &landed],
    );

    // The next loop folds the move and lands on top of it — no
    // ReferenceOutOfDate, no manual branch surgery.
    let session = ws
        .open_session(&agent(), &instruction("land the docs"))
        .unwrap();
    ws.session_write(session.id, "sdk/README.md", "readme\n\nrevised\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );

    // The branch reads whole: the new landing atop the human's rewrite;
    // the superseded first landing is gone from the line.
    assert_eq!(
        git(&sdk, &["log", "--format=%s", "refs/heads/master"]),
        "land the docs\nhuman message\nadopt\nsecond pre-attach commit\nthe pre-attach commit"
    );

    // The fold is journaled as a pull naming the mount and the line.
    let folded = ws.journal(50).unwrap().into_iter().any(|entry| {
        entry.act == Act::Pull
            && entry
                .reference
                .as_deref()
                .is_some_and(|reference| reference.starts_with("sdk "))
    });
    assert!(folded, "no pull act journaled for the fold");
}

#[test]
fn a_rewrite_with_follow_up_work_fast_forwards_the_line() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let sdk = root.path().join("sdk");

    let session = ws
        .open_session(&agent(), &instruction("land the retry"))
        .unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { retry() }\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );

    // A human rewrites the landed commit's message AND keeps working on
    // top of the rewrite before any atelier command runs: the line's
    // content now sits only in an ancestor of the moved tip. Merging
    // against the pre-land base here would refuse spuriously.
    let landed = git(&sdk, &["rev-parse", "refs/heads/master"]);
    let tree = git(&sdk, &["rev-parse", "refs/heads/master^{tree}"]);
    let parent = git(&sdk, &["rev-parse", "refs/heads/master^"]);
    let amended = git(
        &sdk,
        &["commit-tree", &tree, "-p", &parent, "-m", "human message"],
    );
    git(
        &sdk,
        &["update-ref", "refs/heads/master", &amended, &landed],
    );
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", "-q", sdk.to_str().unwrap(), "work"],
    );
    let work = clone.path().join("work");
    git(&work, &["checkout", "-q", "-B", "master", "origin/master"]);
    fs::write(work.join("lib.rs"), "pub fn lib() { retry_harder() }\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "follow-up on the rewrite"]);
    git(&work, &["push", "-q", "origin", "master"]);

    ws.status().unwrap();
    assert_eq!(
        fs::read_to_string(sdk.join("lib.rs")).unwrap(),
        "pub fn lib() { retry_harder() }\n"
    );

    let session = ws
        .open_session(&agent(), &instruction("land the docs"))
        .unwrap();
    ws.session_write(session.id, "sdk/README.md", "readme\n\nrevised\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );

    // The moved history superseded the line whole — no fold state, no
    // duplicate of the landed change, the rewrite and its follow-up
    // verbatim beneath the next landing.
    assert_eq!(
        git(&sdk, &["log", "--format=%s", "refs/heads/master"]),
        "land the docs\nfollow-up on the rewrite\nhuman message\nadopt\nsecond pre-attach commit\nthe pre-attach commit"
    );
}

#[test]
fn a_push_into_the_mount_folds_and_the_session_merges_at_landing() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let sdk = root.path().join("sdk");

    let session = ws
        .open_session(&agent(), &instruction("wire the retry path"))
        .unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { retry() }\n")
        .unwrap();

    // The line moves on its own too: an external edit snapshots, so the
    // fold must merge every line state since the divergence, not the
    // tip's own diff alone.
    fs::write(sdk.join("README.md"), "readme\n\nline note\n").unwrap();
    ws.journal(1).unwrap();

    // Another clone pushes into the mount while the session is open.
    // Before its first landing the mount's HEAD still sits on the
    // adopted branch, so receiving a push needs git's say-so.
    git(&sdk, &["config", "receive.denyCurrentBranch", "ignore"]);
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", "-q", sdk.to_str().unwrap(), "work"],
    );
    let work = clone.path().join("work");
    // The mount's HEAD is detached, so the clone starts detached too;
    // the branch materializes from the remote ref.
    git(&work, &["checkout", "-q", "-B", "master", "origin/master"]);
    fs::write(work.join("upstream.txt"), "pushed content\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "pushed from a clone"]);
    git(&work, &["push", "-q", "origin", "master"]);

    // Any workspace operation folds the push into the line: the mount's
    // files carry both the line's note and the pushed file. The open
    // session stays at its fork point — it merges at landing, exactly
    // as it would had another session landed first.
    ws.status().unwrap();
    assert_eq!(
        fs::read_to_string(sdk.join("upstream.txt")).unwrap(),
        "pushed content\n"
    );
    assert_eq!(
        fs::read_to_string(sdk.join("README.md")).unwrap(),
        "readme\n\nline note\n"
    );
    assert!(
        !session
            .working_copy
            .join("sdk")
            .join("upstream.txt")
            .exists()
    );

    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );
    assert_eq!(
        git(&sdk, &["log", "--format=%s", "refs/heads/master"]),
        "wire the retry path\nfold\npushed from a clone\nsecond pre-attach commit\nthe pre-attach commit"
    );
    assert_eq!(
        fs::read_to_string(sdk.join("lib.rs")).unwrap(),
        "pub fn lib() { retry() }\n"
    );
    assert_eq!(
        fs::read_to_string(sdk.join("upstream.txt")).unwrap(),
        "pushed content\n"
    );
    assert_eq!(
        fs::read_to_string(sdk.join("README.md")).unwrap(),
        "readme\n\nline note\n"
    );
}

#[test]
fn a_push_atop_the_landed_tip_fast_forwards_the_line() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let sdk = root.path().join("sdk");

    let session = ws
        .open_session(&agent(), &instruction("land the retry"))
        .unwrap();
    ws.session_write(session.id, "sdk/lib.rs", "pub fn lib() { retry() }\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );

    // Follow-up work built on the landed tip — even on the very lines
    // the landing wrote — fast-forwards; replaying the line's own diff
    // here would report a conflict that does not exist.
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", "-q", sdk.to_str().unwrap(), "work"],
    );
    let work = clone.path().join("work");
    // The mount's HEAD is detached after the landing; materialize the
    // branch from the remote ref.
    git(&work, &["checkout", "-q", "-B", "master", "origin/master"]);
    fs::write(work.join("lib.rs"), "pub fn lib() { retry_harder() }\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "follow-up on the landing"]);
    git(&work, &["push", "-q", "origin", "master"]);

    ws.status().unwrap();
    assert_eq!(
        fs::read_to_string(sdk.join("lib.rs")).unwrap(),
        "pub fn lib() { retry_harder() }\n"
    );

    let session = ws
        .open_session(&agent(), &instruction("land the docs"))
        .unwrap();
    ws.session_write(session.id, "sdk/README.md", "readme\n\nrevised\n")
        .unwrap();
    let outcome = ws.land(session.id).unwrap();
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );

    // A fast-forward writes no fold state: the pushed commit is the
    // line, and the next landing stacks straight on it.
    assert_eq!(
        git(&sdk, &["log", "--format=%s", "refs/heads/master"]),
        "land the docs\nfollow-up on the landing\nland the retry\nadopt\nsecond pre-attach commit\nthe pre-attach commit"
    );
}

#[test]
fn a_conflicting_out_of_band_move_refuses_by_name_and_resolves() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::init(root.path()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    ws.attach_mount(repo.path(), "sdk").unwrap();
    let sdk = root.path().join("sdk");

    // A clone of the settled state pushes one way while the line edits
    // the same file the other way.
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", "-q", sdk.to_str().unwrap(), "work"],
    );
    let work = clone.path().join("work");
    git(&sdk, &["config", "receive.denyCurrentBranch", "ignore"]);
    fs::write(sdk.join("lib.rs"), "pub fn lib() { line() }\n").unwrap();
    fs::write(work.join("lib.rs"), "pub fn lib() { pushed() }\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "conflicting push"]);
    git(&work, &["push", "-q", "origin", "master"]);

    // The fold refuses by name; nothing on disk is touched.
    let Err(error) = ws.status() else {
        panic!("a conflicting fold must refuse");
    };
    assert!(
        matches!(&error, Error::GitFoldConflicted { branch } if branch == "master"),
        "got: {error:?}"
    );
    assert_eq!(
        fs::read_to_string(sdk.join("lib.rs")).unwrap(),
        "pub fn lib() { line() }\n"
    );

    // Making the working copy agree with the branch resolves: the next
    // operation folds and the workspace works again.
    fs::write(sdk.join("lib.rs"), "pub fn lib() { pushed() }\n").unwrap();
    ws.status().unwrap();
    assert_eq!(
        git(&sdk, &["log", "-1", "--format=%s", "refs/heads/master"]),
        "conflicting push"
    );
}
