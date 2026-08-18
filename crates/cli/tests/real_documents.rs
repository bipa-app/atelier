//! Integration coverage over real Word-produced documents.
//!
//! Synthetic fixtures cannot prove what Word's own output proves: rsid
//! noise, section properties, themes, headers, fields, and whatever else a
//! real authoring session leaves behind. These tests run two genuine
//! document revisions through the whole seam — init, attach, edit, diff —
//! and assert structure, never content, so nothing confidential enters the
//! repository.
//!
//! The fixtures are local-only and gitignored: place two revisions of a
//! real document at `fixtures/real/old.docx` and `fixtures/real/new.docx`
//! (workspace root), then run
//!
//! ```text
//! cargo test -p atelier-ws --test real_documents -- --ignored
//! ```
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use atelier_sdk::{Act, DeltaKind, Fidelity, LineKind, Workspace};
use tempfile::TempDir;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/real")
}

fn command(config_home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("atelier").expect("atelier binary builds");
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(current_dir);
    command
}

fn write_actor_config(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
}

#[test]
#[ignore = "needs local fixtures/real/{old,new}.docx (confidential, gitignored)"]
#[expect(
    unsafe_code,
    reason = "set_var wires the in-process Workspace to the test config"
)]
fn real_word_revisions_diff_at_the_text_rung() {
    let old = fixtures().join("old.docx");
    let new = fixtures().join("new.docx");
    assert!(
        old.is_file() && new.is_file(),
        "place two real document revisions at fixtures/real/old.docx and \
         fixtures/real/new.docx"
    );

    let config_home = TempDir::new().unwrap();
    write_actor_config(config_home.path());
    let workspace = TempDir::new().unwrap();
    let source = TempDir::new().unwrap();
    fs::copy(&old, source.path().join("document.docx")).unwrap();

    command(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();
    command(config_home.path(), workspace.path())
        .args(["attach", source.path().to_str().unwrap()])
        .assert()
        .success();

    fs::copy(&new, workspace.path().join("document.docx")).unwrap();

    // The CLI surface: a real markdown line diff, and the same bytes on a
    // second run — the second serves both sides from the projection cache.
    let first = command(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success();
    let stdout = String::from_utf8(first.get_output().stdout.clone()).unwrap();
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("M document.docx"));
    let (mut removed, mut added) = (0usize, 0usize);
    for line in lines {
        match line.as_bytes().first() {
            Some(b'-') => removed += 1,
            Some(b'+') => added += 1,
            Some(b'\\') => {}
            first => panic!("unexpected diff line start {first:?}: {line:?}"),
        }
    }
    assert!(
        removed > 0 && added > 0,
        "a real revision must produce removed ({removed}) and added ({added}) lines"
    );

    let second = command(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success();
    assert_eq!(
        first.get_output().stdout,
        second.get_output().stdout,
        "the cached diff must be byte-identical"
    );

    // The SDK surface: the delta sits at the text rung, stamped with the
    // docx package, and the projector degraded nothing — the journal must
    // hold no package_failed or file_too_large act.
    // SAFETY: the only test in this binary; nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home.path());
    }
    let mut ws = Workspace::open(workspace.path()).unwrap();
    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    let delta = &diff.deltas[0];
    assert_eq!(delta.kind, DeltaKind::Changed);
    assert_eq!(delta.fidelity, Fidelity::Text);
    assert_eq!(
        delta.package.map(|package| package.name),
        Some("format-docx")
    );
    assert!(
        delta
            .lines
            .iter()
            .any(|line| matches!(line.kind, LineKind::Removed | LineKind::Added)),
        "the delta must carry changed lines"
    );

    let entries = ws.journal(200).unwrap();
    let degraded: Vec<_> = entries
        .iter()
        .filter(|entry| matches!(entry.act, Act::PackageFailed | Act::FileTooLarge))
        .collect();
    assert!(
        degraded.is_empty(),
        "real documents must project at full fidelity, got: {degraded:#?}"
    );
}
