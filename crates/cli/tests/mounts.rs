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

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", "Atelier Test")
        .env("GIT_AUTHOR_EMAIL", "atelier@example.invalid")
        .env("GIT_COMMITTER_NAME", "Atelier Test")
        .env("GIT_COMMITTER_EMAIL", "atelier@example.invalid")
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git fixture output is utf-8")
        .trim()
        .to_owned()
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

#[test]
fn dirty_git_sources_refuse_until_the_actor_accepts_the_preflight() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create workspace tempdir");
    atelier(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    let source = TempDir::new().expect("create Git source");
    git(source.path(), &["init", "-q", "-b", "main"]);
    fs::write(source.path().join("lib.rs"), "pub fn clean() {}\n").expect("write tracked file");
    git(source.path(), &["add", "lib.rs"]);
    git(source.path(), &["commit", "-qm", "initial source"]);
    let head = git(source.path(), &["rev-parse", "HEAD"]);
    fs::write(source.path().join("lib.rs"), "pub fn dirty() {}\n").expect("modify tracked file");
    fs::write(source.path().join("scratch.bin"), b"123456").expect("write untracked file");

    let preflight = format!(
        "source git: HEAD {head}; branch main\nsource git state: tracked modifications: 1; untracked files: 1; estimated untracked bytes: 6\n"
    );
    atelier(config_home.path(), workspace.path())
        .args(["attach", source.path().to_str().expect("utf-8 source path")])
        .args(["--mount", "sdk"])
        .assert()
        .failure()
        .stdout(preflight.clone())
        .stderr(
            "error: local Git source is dirty; attach a clean clone or pass --allow-dirty to adopt these changes\n",
        );
    assert!(
        !workspace.path().join("sdk").exists(),
        "a refused preflight must not create the mount"
    );

    atelier(config_home.path(), workspace.path())
        .args(["attach", source.path().to_str().expect("utf-8 source path")])
        .args(["--mount", "sdk", "--allow-dirty"])
        .assert()
        .success()
        .stdout(format!(
            "{preflight}warning: --allow-dirty adopts the reported tracked and untracked changes\nattached local-git {} at sdk\n",
            source.path().display()
        ));
    assert_eq!(
        fs::read_to_string(workspace.path().join("sdk/lib.rs")).expect("read adopted tracked file"),
        "pub fn dirty() {}\n"
    );
    assert_eq!(
        fs::read(workspace.path().join("sdk/scratch.bin")).expect("read adopted untracked file"),
        b"123456"
    );
}

#[test]
fn linked_worktrees_refuse_with_a_clone_route_that_keeps_their_committed_head() {
    for bare_owner in [false, true] {
        let fixture = TempDir::new().expect("create fixture");
        let fixture_root = fixture
            .path()
            .canonicalize()
            .expect("canonical fixture path");
        let config_home = fixture_root.join("config");
        write_actor_config(&config_home);
        let workspace = fixture_root.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");
        atelier(&config_home, &workspace)
            .arg("init")
            .assert()
            .success();

        let source = fixture_root.join("source");
        fs::create_dir(&source).expect("create source");
        git(&source, &["init", "-q", "-b", "main"]);
        fs::write(source.join("branch.txt"), "main content\n").expect("write main content");
        git(&source, &["add", "branch.txt"]);
        git(&source, &["commit", "-qm", "main content"]);
        let owner = if bare_owner {
            git(&fixture_root, &["clone", "--bare", "source", "owner.git"]);
            fixture_root.join("owner.git")
        } else {
            source.clone()
        };
        let linked = fixture_root.join("card-worktree");
        git(
            &owner,
            &[
                "worktree",
                "add",
                "-b",
                "example",
                linked.to_str().expect("utf-8 linked path"),
            ],
        );
        fs::write(linked.join("branch.txt"), "card content\n").expect("write card content");
        git(&linked, &["commit", "-am", "card content"]);
        let head = git(&linked, &["rev-parse", "HEAD"]);
        let error = if bare_owner {
            format!(
                "error: config error: {} carries a .git file pointing at {}; only a repository owning its .git directory attaches; clone the committed HEAD first: git clone --no-local --single-branch -- <worktree> <new-source>, then atelier attach <new-source> --mount <name>; cloning does not copy uncommitted edits\n",
                linked.display(),
                owner.join("worktrees/card-worktree").display()
            )
        } else {
            format!(
                "error: {} is a linked git worktree of {}; linked worktrees must be cloned before attachment: git clone --no-local --single-branch -- <worktree> <new-source>, then atelier attach <new-source> --mount <name>; cloning copies the worktree's committed HEAD, not uncommitted edits\n",
                linked.display(),
                owner.display()
            )
        };
        atelier(&config_home, &workspace)
            .args([
                "attach",
                linked.to_str().expect("utf-8 linked path"),
                "--mount",
                "app",
            ])
            .assert()
            .failure()
            .stdout("")
            .stderr(error);
        assert!(!workspace.join("app").exists());
        assert_eq!(git(&source, &["show", "main:branch.txt"]), "main content");
        fs::write(linked.join("branch.txt"), "uncommitted content\n")
            .expect("write an edit outside the clone contract");

        git(
            &fixture_root,
            &[
                "clone",
                "--no-local",
                "--single-branch",
                "--",
                "card-worktree",
                "card-source",
            ],
        );
        let standalone = fixture_root.join("card-source");
        assert!(standalone.join(".git").is_dir());
        assert!(!standalone.join(".git/objects/info/alternates").exists());
        assert_eq!(git(&standalone, &["branch", "--show-current"]), "example");
        assert_eq!(git(&standalone, &["rev-parse", "HEAD"]), head);
        atelier(&config_home, &workspace)
            .args([
                "attach",
                standalone.to_str().expect("utf-8 standalone path"),
                "--mount",
                "app",
            ])
            .assert()
            .success()
            .stdout(format!(
                "source git: HEAD {head}; branch example\nsource git state: tracked modifications: 0; untracked files: 0; estimated untracked bytes: 0\nattached local-git {} at app\n",
                standalone.display()
            ))
            .stderr("");
        assert!(!workspace.join("app/.git/objects/info/alternates").exists());
        assert_eq!(
            fs::read_to_string(workspace.join("app/branch.txt")).expect("read attached content"),
            "card content\n"
        );
        assert_eq!(
            fs::read_to_string(linked.join("branch.txt")).expect("read original edit"),
            "uncommitted content\n"
        );
    }
}
