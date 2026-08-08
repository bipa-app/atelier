use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn command(config_home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("ws").unwrap();
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(current_dir);
    command
}

fn write_actor_config(config_home: &Path) {
    fs::create_dir_all(config_home).unwrap();
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .unwrap();
}

/// The path a `ws` process run in `dir` reports: its canonicalized cwd.
fn canonical(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap()
}

fn stdout_lines(assert: &assert_cmd::assert::Assert) -> Vec<String> {
    String::from_utf8(assert.get_output().stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn assert_line_matches(line: &str, pattern: &str) {
    let matched = predicate::str::is_match(pattern).unwrap().eval(line);
    assert!(matched, "line {line:?} does not match {pattern:?}");
}

const TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z";
const SNAPSHOT_ID: &str = "[0-9a-f]{40}";

#[test]
fn workspace_round_trip_works_through_the_cli() {
    let config_home = TempDir::new().unwrap();
    write_actor_config(config_home.path());
    let workspace = TempDir::new().unwrap();
    let source = TempDir::new().unwrap();
    fs::write(source.path().join("notes.txt"), "hello").unwrap();
    fs::write(source.path().join("data.bin"), [0_u8, 1, 2, 3]).unwrap();

    let root = canonical(workspace.path());
    command(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success()
        .stdout(format!(
            "initialized workspace {} at {}\n",
            root.file_name().unwrap().to_str().unwrap(),
            root.display()
        ));

    command(config_home.path(), workspace.path())
        .args(["attach", source.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(format!(
            "attached local-folder {}\n",
            source.path().display()
        ));
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        fs::read(workspace.path().join("data.bin")).unwrap(),
        [0_u8, 1, 2, 3]
    );

    fs::write(workspace.path().join("notes.txt"), "hello world").unwrap();

    let journal = command(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    let lines = stdout_lines(&journal);
    assert_eq!(lines.len(), 3, "unexpected journal: {lines:#?}");
    assert_line_matches(
        &lines[0],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  snapshot  {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        &lines[1],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  source_attach  {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        &lines[2],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  workspace_init$"),
    );

    command(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success()
        .stdout("M notes.txt\n");

    let not_a_workspace = TempDir::new().unwrap();
    command(config_home.path(), not_a_workspace.path())
        .arg("diff")
        .assert()
        .failure()
        .stderr(format!(
            "error: not a workspace: {}\n",
            canonical(not_a_workspace.path()).display()
        ));

    let empty_config_home = TempDir::new().unwrap();
    command(empty_config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .failure()
        .stderr(
            "error: no actor is configured: create ~/.config/atelier/config.toml \
             with [actor] name = \"you\" kind = \"human\"\n",
        );
}
