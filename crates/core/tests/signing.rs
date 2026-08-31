//! Publishing identity and signing (ADR-0015): with `[git]` configured,
//! every commit atelier writes carries the identity as committer; the
//! owning human authors as the identity while agents author as
//! themselves; with `[git.signing]` configured, the identity's ssh key
//! signs everything and `git` verifies it. Without `[git]`, the
//! synthetic per-actor address stands and nothing signs.

use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use atelier_sdk::{Actor, ActorKind, GateOutcome, Instruction, Workspace};

/// Serialize tests: they all set process-wide environment variables.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[expect(unsafe_code, reason = "set_var wires the workspace to the test config")]
fn set_config(config_home: &Path, body: &str) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(config_home.join("config.toml"), body).expect("write config");
    // SAFETY: every test holds `env_lock()` for its whole body, so no other
    // thread reads or writes the environment concurrently.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

fn human(name: &str) -> Actor {
    Actor {
        name: name.to_owned(),
        kind: ActorKind::Human,
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
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("git output is utf-8")
        .trim()
        .to_owned()
}

/// A throwaway ed25519 keypair for signing; returns the private key path
/// and the `allowed_signers` line `git` verifies the email against.
fn generate_key(dir: &Path, email: &str) -> (String, String) {
    let key = dir.join("key");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "atelier-test"])
        .arg("-f")
        .arg(&key)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen failed");
    let public = fs::read_to_string(dir.join("key.pub")).expect("read public key");
    let mut fields = public.split_whitespace();
    let kind = fields.next().expect("key type");
    let blob = fields.next().expect("key blob");
    (
        key.display().to_string(),
        format!("{email} {kind} {blob}\n"),
    )
}

fn land_note(workspace: &mut Workspace, actor: &Actor, path: &str) {
    let session = workspace
        .open_session(actor, &instruction("land the note"))
        .expect("open the session");
    workspace
        .session_write(session.id, path, "note\n")
        .expect("write the note");
    let outcome = workspace.land(session.id).expect("land the session");
    assert!(
        matches!(outcome, GateOutcome::Landed { .. }),
        "got: {outcome:?}"
    );
}

#[test]
fn a_configured_identity_signs_landed_commits_verifiably() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    let (key, signer_line) = generate_key(config.path(), "luiz@example.com");
    set_config(
        config.path(),
        &format!(
            "[actor]\nname = \"luiz\"\nkind = \"human\"\n\n\
             [git]\nname = \"Luiz Parreira\"\nemail = \"luiz@example.com\"\n\n\
             [git.signing]\nbackend = \"ssh\"\nkey = \"{key}\"\n"
        ),
    );
    let root = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::init(root.path()).unwrap();
    land_note(&mut workspace, &human("luiz"), "note.txt");

    // The landed commit carries the publishing identity whole: the
    // session actor "luiz" is the owning human, so author and committer
    // agree — and the signature verifies against the configured key.
    assert_eq!(
        git(
            root.path(),
            &[
                "log",
                "-1",
                "--format=%an %ae %cn %ce",
                "refs/heads/atelier"
            ]
        ),
        "Luiz Parreira luiz@example.com Luiz Parreira luiz@example.com"
    );
    let signers = config.path().join("allowed_signers");
    fs::write(&signers, signer_line).unwrap();
    let signers = signers.display().to_string();
    assert_eq!(
        git(
            root.path(),
            &[
                "-c",
                &format!("gpg.ssh.allowedSignersFile={signers}"),
                "log",
                "-1",
                "--format=%G?",
                "refs/heads/atelier",
            ]
        ),
        "G"
    );
}

#[test]
fn an_agent_authored_commit_carries_the_publishing_committer() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    let (key, signer_line) = generate_key(config.path(), "luiz@example.com");
    set_config(
        config.path(),
        &format!(
            "[actor]\nname = \"luiz\"\nkind = \"human\"\n\n\
             [git]\nname = \"Luiz Parreira\"\nemail = \"luiz@example.com\"\n\n\
             [git.signing]\nbackend = \"ssh\"\nkey = \"{key}\"\n"
        ),
    );
    let root = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::init(root.path()).unwrap();
    land_note(&mut workspace, &agent("codex"), "note.txt");

    // The agent stays the author — its work is attributed — while the
    // committer and the signature are the publishing identity's: made by
    // the agent, published and vouched for by the owner.
    assert_eq!(
        git(
            root.path(),
            &[
                "log",
                "-1",
                "--format=%an %ae %cn %ce",
                "refs/heads/atelier"
            ]
        ),
        "codex codex@atelier.local Luiz Parreira luiz@example.com"
    );
    let signers = config.path().join("allowed_signers");
    fs::write(&signers, signer_line).unwrap();
    let signers = signers.display().to_string();
    assert_eq!(
        git(
            root.path(),
            &[
                "-c",
                &format!("gpg.ssh.allowedSignersFile={signers}"),
                "log",
                "-1",
                "--format=%G?",
                "refs/heads/atelier",
            ]
        ),
        "G"
    );
}

#[test]
fn without_an_identity_the_synthetic_address_stands_unsigned() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_config(
        config.path(),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    );
    let root = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::init(root.path()).unwrap();
    land_note(&mut workspace, &human("test-actor"), "note.txt");

    assert_eq!(
        git(
            root.path(),
            &[
                "log",
                "-1",
                "--format=%an %ae %cn %ce",
                "refs/heads/atelier"
            ]
        ),
        "test-actor test-actor@atelier.local test-actor test-actor@atelier.local"
    );
    let raw = git(root.path(), &["cat-file", "commit", "refs/heads/atelier"]);
    assert!(!raw.contains("gpgsig"), "unexpected signature:\n{raw}");
}

#[test]
fn an_empty_publishing_email_refuses() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_config(
        config.path(),
        "[actor]\nname = \"luiz\"\nkind = \"human\"\n\n\
         [git]\nname = \"Luiz Parreira\"\nemail = \"\"\n",
    );
    let root = tempfile::tempdir().unwrap();
    let Err(error) = Workspace::init(root.path()) else {
        panic!("an empty git.email must refuse");
    };
    assert_eq!(
        error.to_string(),
        "config error: git.email must not be empty"
    );
}
