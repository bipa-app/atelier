//! Mounted sources end to end: two projects with their own histories under
//! one workspace, read through the human face (ADR-0009, N0).
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z";
const SNAPSHOT_ID: &str = "[0-9a-f]{40}";

fn write_actor_config(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
}

fn atelier(config_home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("atelier").expect("atelier binary builds");
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(current_dir);
    command
}

fn stdout_lines(assert: &assert_cmd::assert::Assert) -> Vec<String> {
    String::from_utf8(assert.get_output().stdout.clone())
        .expect("stdout is utf-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn assert_line_matches(line: &str, pattern: &str) {
    let matched = predicate::str::is_match(pattern)
        .expect("valid pattern")
        .eval(line);
    assert!(matched, "line {line:?} does not match {pattern:?}");
}

#[test]
fn two_projects_live_under_one_workspace_with_their_own_histories() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create workspace tempdir");
    atelier(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    let app = TempDir::new().expect("create app source");
    fs::write(app.path().join("main.rs"), "fn main() {}\n").expect("write app file");
    let docs = TempDir::new().expect("create docs source");
    fs::write(docs.path().join("guide.md"), "# Guide\n").expect("write docs file");

    atelier(config_home.path(), workspace.path())
        .args(["attach", app.path().to_str().expect("utf-8 path")])
        .args(["--mount", "app"])
        .assert()
        .success()
        .stdout(format!(
            "attached local-folder {} at app\n",
            app.path().display()
        ));
    atelier(config_home.path(), workspace.path())
        .args(["attach", docs.path().to_str().expect("utf-8 path")])
        .args(["--mount", "docs"])
        .assert()
        .success();

    // One edit per source: the root, and both mounts.
    fs::write(workspace.path().join("plan.md"), "# The plan\n").expect("write root file");
    atelier(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    fs::write(workspace.path().join("plan.md"), "# The plan\n\nrevised\n")
        .expect("revise root file");
    fs::write(
        workspace.path().join("app").join("main.rs"),
        "fn main() { run() }\n",
    )
    .expect("edit app file");
    fs::write(
        workspace.path().join("docs").join("guide.md"),
        "# Guide\n\nmore\n",
    )
    .expect("edit docs file");

    // The aggregate diff: root unprefixed, mounts scoped, in mount order.
    let diff = atelier(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success();
    assert_eq!(
        stdout_lines(&diff),
        vec![
            "M plan.md",
            "+",
            "+revised",
            "M app/main.rs",
            "-fn main() {}",
            "+fn main() { run() }",
            "M docs/guide.md",
            "+",
            "+more",
        ],
    );

    // Three histories: root lines keep the exact v1 shape, mounted lines
    // carry their mount, and no snapshot id repeats across sources.
    let history = atelier(config_home.path(), workspace.path())
        .arg("history")
        .assert()
        .success();
    let lines = stdout_lines(&history);
    let root_lines: Vec<&String> = lines
        .iter()
        .filter(|line| line.split("  ").next().is_some_and(|id| id.len() == 40))
        .collect();
    let app_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with("app  ")).collect();
    let docs_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with("docs  ")).collect();
    assert!(!root_lines.is_empty(), "no root history: {lines:#?}");
    assert_eq!(app_lines.len(), 3, "app history: {lines:#?}");
    assert_eq!(docs_lines.len(), 3, "docs history: {lines:#?}");
    assert_eq!(
        lines.len(),
        root_lines.len() + app_lines.len() + docs_lines.len(),
        "unaccounted history lines: {lines:#?}"
    );
    for line in &app_lines {
        assert_line_matches(
            line,
            &format!("^app  {SNAPSHOT_ID}  test-actor  {TIMESTAMP}$"),
        );
    }

    // The journal names the mounted acts: each attach with its mount and
    // snapshot, each mounted snapshot with its mount.
    let journal = atelier(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    let entries = stdout_lines(&journal);
    let attach_lines: Vec<&String> = entries
        .iter()
        .filter(|line| line.contains("source_attach"))
        .collect();
    assert_eq!(attach_lines.len(), 2, "journal: {entries:#?}");
    assert_line_matches(
        attach_lines[1],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  source_attach  app {SNAPSHOT_ID}$"),
    );
    assert!(
        entries
            .iter()
            .any(|line| line.contains("snapshot  docs ") || line.contains("snapshot  docs")),
        "no mounted snapshot act: {entries:#?}"
    );
}
