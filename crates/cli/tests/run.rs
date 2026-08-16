//! `atelier run` end to end: any command works inside a session's working
//! copy; its edits are the session's change, never the shared line's, until
//! a landing passes the gate.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

fn write_actor_config(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
}

fn ws(config_home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("atelier").expect("atelier binary builds");
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(current_dir);
    command
}

fn stdout(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is utf-8")
}

#[test]
fn run_carries_edits_into_a_session_not_the_shared_line() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create temp workspace");
    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    let run = ws(config_home.path(), workspace.path())
        .args([
            "run",
            "--summary",
            "add the notes",
            "--",
            "sh",
            "-c",
            "echo hello > notes.txt",
        ])
        .assert()
        .success();
    assert_eq!(
        stdout(&run),
        "A notes.txt\nsession s1 holds the change; land with: atelier land s1\n"
    );

    // The shared line never saw the edit; the session holds it.
    assert!(!workspace.path().join("notes.txt").exists());

    let land = ws(config_home.path(), workspace.path())
        .args(["land", "s1"])
        .assert()
        .success();
    assert!(
        stdout(&land).starts_with("landed "),
        "got: {}",
        stdout(&land)
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes.txt")).expect("landed file"),
        "hello\n"
    );
}

#[test]
fn run_land_lands_the_change_on_success() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create temp workspace");
    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    let run = ws(config_home.path(), workspace.path())
        .args(["run", "--land", "--", "sh", "-c", "echo done > a.txt"])
        .assert()
        .success();
    let output = stdout(&run);
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[0], "A a.txt");
    assert!(lines[1].starts_with("landed "), "got: {output}");
    assert_eq!(
        fs::read_to_string(workspace.path().join("a.txt")).expect("landed file"),
        "done\n"
    );

    // The journal attributes the whole arc: session open through land.
    let journal = ws(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    let journal = stdout(&journal);
    assert!(
        journal.contains("land") && journal.contains("session_open"),
        "got: {journal}"
    );
}

#[test]
fn a_failing_command_keeps_the_session_and_its_work() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create temp workspace");
    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    let run = ws(config_home.path(), workspace.path())
        .args(["run", "--", "sh", "-c", "echo partial > b.txt; exit 3"])
        .assert()
        .failure();
    let stderr = String::from_utf8(run.get_output().stderr.clone()).expect("stderr is utf-8");
    assert!(
        stderr.contains("session s1 keeps the work - land with: atelier land s1"),
        "got: {stderr}"
    );

    // Nothing landed; the session survived with the edit versioned.
    assert!(!workspace.path().join("b.txt").exists());
    let sessions = ws(config_home.path(), workspace.path())
        .arg("sessions")
        .assert()
        .success();
    assert!(
        stdout(&sessions).contains("s1"),
        "got: {}",
        stdout(&sessions)
    );
    let diff = ws(config_home.path(), workspace.path())
        .args(["run", "--summary", "resume check", "--", "sh", "-c", "true"])
        .assert()
        .success();
    // A later run opens its own session; the failed one still holds b.txt.
    assert!(
        stdout(&diff).contains("session s2"),
        "got: {}",
        stdout(&diff)
    );
}
