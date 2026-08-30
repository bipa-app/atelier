//! `atelier update` end to end: the verb launches the bundled updater
//! beside the binary, forwards a failure verbatim, and — with no updater
//! present — refuses with the install move.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

/// Copy the built binary into `dir` so the updater lookup resolves
/// against a directory the test controls.
fn install_binary(dir: &Path) -> PathBuf {
    let binary = dir.join("atelier");
    fs::copy(env!("CARGO_BIN_EXE_atelier"), &binary).expect("copy atelier binary");
    binary
}

fn install_updater_stub(dir: &Path, script: &str) {
    let stub = dir.join("atelier-ws-update");
    fs::write(&stub, script).expect("write updater stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("mark stub executable");
}

#[test]
fn update_runs_the_updater_beside_the_binary() {
    let bin_dir = TempDir::new().expect("create bin tempdir");
    let binary = install_binary(bin_dir.path());
    install_updater_stub(bin_dir.path(), "#!/bin/sh\necho stub updater ran\n");

    Command::new(binary)
        .arg("update")
        .assert()
        .success()
        .stdout("stub updater ran\n");
}

#[test]
fn a_failing_updater_fails_the_update() {
    let bin_dir = TempDir::new().expect("create bin tempdir");
    let binary = install_binary(bin_dir.path());
    install_updater_stub(bin_dir.path(), "#!/bin/sh\nexit 7\n");

    Command::new(binary)
        .arg("update")
        .assert()
        .failure()
        .stderr("error: the updater failed (exit status: 7)\n");
}

#[test]
fn update_without_a_bundled_updater_teaches_the_install_move() {
    // The build directory carries the binary alone; only the install
    // script places an updater beside it.
    Command::cargo_bin("atelier")
        .expect("atelier binary builds")
        .arg("update")
        .assert()
        .failure()
        .stderr(
            "error: this install has no bundled updater; re-run the install script \
             (curl -fsSL https://atelier-ws.dev/install.sh | sh) or, for cargo installs: \
             cargo install atelier-ws --force\n",
        );
}
