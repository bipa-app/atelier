//! The README quickstart, run verbatim: the block is extracted from
//! README.md itself, so the documented commands and the tested commands
//! cannot drift apart.

use std::fs;
use std::path::Path;
use std::process::Command;

use predicates::prelude::*;
use tempfile::TempDir;

/// Every fenced `sh` block in the README's Quickstart section, in order.
fn quickstart_blocks(readme: &str) -> Vec<String> {
    let section = readme
        .split_once("## Quickstart")
        .expect("README has a Quickstart section")
        .1;
    let section = match section.split_once("\n## ") {
        Some((section, _)) => section,
        None => section,
    };
    section
        .split("```sh\n")
        .skip(1)
        .map(|fenced| {
            fenced
                .split_once("```")
                .expect("fenced block closes")
                .0
                .to_owned()
        })
        .collect()
}

fn assert_line_matches(line: &str, pattern: &str) {
    let matched = predicate::str::is_match(pattern)
        .expect("valid pattern")
        .eval(line);
    assert!(matched, "line {line:?} does not match {pattern:?}");
}

const TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z";
const SNAPSHOT_ID: &str = "[0-9a-f]{40}";

#[test]
fn the_readme_quickstart_runs_end_to_end_in_a_temp_dir() {
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))
        .expect("read README.md");
    let blocks = quickstart_blocks(&readme);
    assert_eq!(
        blocks.len(),
        2,
        "the Quickstart holds a build block and a session block"
    );
    assert!(
        blocks[0].contains("cargo install"),
        "the first block builds the binary; the test substitutes the one cargo already built"
    );
    let session = &blocks[1];

    // A stranger's machine: an empty home, a fresh working directory, and
    // `ws` on PATH — the one substitution for the `cargo install` block.
    let home = TempDir::new().expect("create home tempdir");
    let cwd = TempDir::new().expect("create working tempdir");
    let ws_dir = Path::new(env!("CARGO_BIN_EXE_ws"))
        .parent()
        .expect("ws binary has a parent dir");
    let path = format!(
        "{}:{}",
        ws_dir.display(),
        std::env::var("PATH").expect("PATH is set")
    );

    let output = Command::new("sh")
        .args(["-euc", session])
        .current_dir(cwd.path())
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", path)
        .output()
        .expect("run the quickstart session");

    assert!(
        output.status.success(),
        "quickstart failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "unexpected quickstart output: {lines:#?}");
    let demo = fs::canonicalize(cwd.path().join("demo")).expect("canonicalize demo dir");
    assert_eq!(
        lines[0],
        format!("initialized workspace demo at {}", demo.display())
    );
    assert_line_matches(
        lines[1],
        &format!("^{TIMESTAMP}  you \\(human\\)  snapshot  {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        lines[2],
        &format!("^{TIMESTAMP}  you \\(human\\)  workspace_init$"),
    );
    assert_eq!(lines[3], "M notes.txt");
    assert_eq!(lines[4], "+no save button, no lost work");
}
