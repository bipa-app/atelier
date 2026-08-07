use std::fs;
use std::path::Path;

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

#[test]
fn workspace_round_trip_works_through_the_cli() {
    let config_home = TempDir::new().unwrap();
    write_actor_config(config_home.path());
    let workspace = TempDir::new().unwrap();
    let source = TempDir::new().unwrap();
    fs::write(source.path().join("notes.txt"), "hello").unwrap();
    fs::write(source.path().join("data.bin"), [0_u8, 1, 2, 3]).unwrap();

    command(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized workspace"));

    command(config_home.path(), workspace.path())
        .args(["attach", source.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("attached local-folder"));
    assert!(workspace.path().join("notes.txt").is_file());

    fs::write(workspace.path().join("notes.txt"), "hello world").unwrap();

    command(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success()
        .stdout(predicate::str::contains("test-actor").and(predicate::str::contains("snapshot")));

    command(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success()
        .stdout(predicate::str::contains("M notes.txt"))
        .stdout(predicate::str::contains(".atelier").not())
        .stdout(predicate::str::contains(".jj").not())
        .stdout(predicate::str::contains(".git").not());

    let not_a_workspace = TempDir::new().unwrap();
    command(config_home.path(), not_a_workspace.path())
        .arg("diff")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a workspace"));

    let empty_config_home = TempDir::new().unwrap();
    command(empty_config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .failure()
        .stderr(predicate::str::contains("config.toml"));
}
