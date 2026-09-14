//! Integration clients can detect the publishing policy before writing config.
//! Probe from an empty directory with invalid identity config and prove no writes.

use std::fs;
use std::process::Command;

#[test]
fn capabilities_needs_no_workspace_or_identity_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::write(config.path().join("config.toml"), "not valid TOML").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_atelier"))
        .arg("capabilities")
        .current_dir(root.path())
        .env("ATELIER_CONFIG_HOME", config.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stderr, b"");
    assert_eq!(
        output.stdout,
        b"{\"schema\":1,\"features\":[\"git-author-publisher\"]}\n"
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(config.path().join("config.toml")).unwrap(),
        "not valid TOML"
    );
}
