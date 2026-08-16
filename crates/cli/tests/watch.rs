//! `atelier watch` as a process: external edits become attributed
//! snapshots after the debounce, and a restart catches up cleanly.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use predicates::prelude::*;
use tempfile::TempDir;

/// The acceptance bound: an external edit becomes a snapshot within this.
const BOUND: Duration = Duration::from_secs(5);

const TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z";

fn write_actor_config(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
}

/// The path a `atelier` process run in `dir` reports: its canonicalized cwd.
fn canonical(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).expect("canonicalize test dir")
}

/// A running `atelier watch` child whose stdout arrives line by line.
struct RunningWatch {
    child: Child,
    lines: Receiver<String>,
}

impl RunningWatch {
    fn spawn(config_home: &Path, workspace: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_atelier"))
            .args(["watch", "--debounce-ms", "200"])
            .env("ATELIER_CONFIG_HOME", config_home)
            .current_dir(workspace)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn atelier watch");
        let stdout = child.stdout.take().expect("watch stdout is piped");
        Self {
            child,
            lines: read_lines(stdout),
        }
    }

    fn next_line(&self) -> String {
        self.lines
            .recv_timeout(BOUND)
            .expect("a watch line within the bound")
    }

    /// The id from the next line, which must be `snapshot <id>`.
    fn next_snapshot(&self) -> String {
        let line = self.next_line();
        assert!(
            line.starts_with("snapshot "),
            "expected a snapshot line, got {line:?}"
        );
        line["snapshot ".len()..].to_owned()
    }

    fn kill(mut self) {
        self.child.kill().expect("kill atelier watch");
        self.child.wait().expect("reap atelier watch");
    }
}

fn read_lines(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                return;
            };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

fn ws(config_home: &Path, current_dir: &Path) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::cargo_bin("atelier").expect("atelier binary builds");
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(current_dir);
    command
}

fn assert_line_matches(line: &str, pattern: &str) {
    let matched = predicate::str::is_match(pattern)
        .expect("valid pattern")
        .eval(line);
    assert!(matched, "line {line:?} does not match {pattern:?}");
}

#[test]
fn watch_snapshots_external_edits_and_catches_up_after_a_stop() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create workspace tempdir");
    let root = canonical(workspace.path());

    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    // An edit made before any watcher runs: the catch-up scan owns it.
    fs::write(workspace.path().join("notes.txt"), "one\n").expect("write notes");

    let watch = RunningWatch::spawn(config_home.path(), workspace.path());
    assert_eq!(watch.next_line(), format!("watching {}", root.display()));
    let caught_up = watch.next_snapshot();

    // A live external edit: snapshotted within the bound, no atelier command.
    fs::write(workspace.path().join("notes.txt"), "one\ntwo\n").expect("append notes");
    let live = watch.next_snapshot();
    watch.kill();

    // With the watcher stopped, an edit stays outstanding — the restarted
    // watcher's catch-up scan snapshots it, which could not happen had
    // anything snapshotted it in between.
    fs::write(workspace.path().join("notes.txt"), "one\ntwo\nthree\n").expect("append notes");
    let watch = RunningWatch::spawn(config_home.path(), workspace.path());
    assert_eq!(watch.next_line(), format!("watching {}", root.display()));
    let restarted = watch.next_snapshot();
    watch.kill();

    let journal = ws(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    let lines: Vec<String> = String::from_utf8(journal.get_output().stdout.clone())
        .expect("stdout is utf-8")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 4, "unexpected journal: {lines:#?}");
    assert_line_matches(
        &lines[0],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  snapshot  {restarted}$"),
    );
    assert_line_matches(
        &lines[1],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  snapshot  {live}$"),
    );
    assert_line_matches(
        &lines[2],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  snapshot  {caught_up}$"),
    );
    assert_line_matches(
        &lines[3],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  workspace_init$"),
    );
}
