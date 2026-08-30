//! The human CLI round trip in a fresh temp dir: init, attach, edit,
//! diff, journal — output asserted exactly, line by line.
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

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

/// The path a `atelier` process run in `dir` reports: its canonicalized cwd.
fn canonical(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).expect("canonicalize test dir")
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
        .stdout(
            "M notes.txt\n-hello\n\\ no newline at end of file\n\
             +hello world\n\\ no newline at end of file\n",
        );

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

#[test]
fn init_in_git_repository_refuses_with_attach_instructions() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let repository = TempDir::new().expect("create repository tempdir");
    fs::create_dir(repository.path().join(".git")).expect("create git metadata");
    let root = canonical(repository.path());

    command(config_home.path(), &root)
        .arg("init")
        .assert()
        .failure()
        .stderr(format!(
            "error: cannot initialize a workspace at {root}: it is already a git repository; \
             initialize a workspace elsewhere, then run: atelier attach {root} --mount <name>\n",
            root = root.display()
        ));

    assert!(!root.join(".atelier").exists());
    assert!(root.join(".git").exists());
}

#[test]
fn an_empty_control_marker_neither_opens_nor_blocks_init() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let stray = TempDir::new().expect("create stray tempdir");
    fs::create_dir(stray.path().join(".atelier")).expect("create empty marker");
    let root = canonical(stray.path());

    // No config file, no workspace: reads refuse by name instead of the
    // io error the raw marker used to produce.
    command(config_home.path(), &root)
        .arg("status")
        .assert()
        .failure()
        .stderr(format!("error: not a workspace: {}\n", root.display()));

    // The stray marker does not block a fresh init from repairing it.
    command(config_home.path(), &root)
        .arg("init")
        .assert()
        .success()
        .stdout(format!(
            "initialized workspace {} at {}\n",
            root.file_name()
                .expect("dir has a name")
                .to_str()
                .expect("utf-8 name"),
            root.display()
        ));
    command(config_home.path(), &root)
        .arg("sessions")
        .assert()
        .success()
        .stdout("no sessions\n");
}

#[test]
fn an_empty_control_marker_in_a_git_repository_teaches_the_attach_move() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let repository = TempDir::new().expect("create repository tempdir");
    fs::create_dir(repository.path().join(".git")).expect("create git metadata");
    fs::create_dir(repository.path().join(".atelier")).expect("create empty marker");
    let root = canonical(repository.path());

    // The stray marker no longer claims "a workspace already exists";
    // the git refusal names the working setup instead.
    command(config_home.path(), &root)
        .arg("init")
        .assert()
        .failure()
        .stderr(format!(
            "error: cannot initialize a workspace at {root}: it is already a git repository; \
             initialize a workspace elsewhere, then run: atelier attach {root} --mount <name>\n",
            root = root.display()
        ));
}

const DOCX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const DOCX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

/// A one-paragraph Word document holding `sentence`, zipped as a .docx.
fn docx(sentence: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{sentence}</w:t></w:r></w:p></w:body></w:document>"#
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer
        .start_file("[Content_Types].xml", options)
        .expect("start fixture part");
    writer
        .write_all(DOCX_CONTENT_TYPES.as_bytes())
        .expect("write fixture part");
    writer
        .start_file("_rels/.rels", options)
        .expect("start fixture part");
    writer
        .write_all(DOCX_RELS.as_bytes())
        .expect("write fixture part");
    writer
        .start_file("word/document.xml", options)
        .expect("start fixture part");
    writer
        .write_all(document.as_bytes())
        .expect("write fixture part");
    writer
        .finish()
        .expect("finish fixture archive")
        .into_inner()
}

#[test]
fn a_changed_docx_diffs_as_a_markdown_line_diff() {
    let config_home = TempDir::new().unwrap();
    write_actor_config(config_home.path());
    let workspace = TempDir::new().unwrap();
    let source = TempDir::new().unwrap();
    fs::write(
        source.path().join("report.docx"),
        docx("The quick brown fox jumps over the lazy dog."),
    )
    .unwrap();

    command(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();
    command(config_home.path(), workspace.path())
        .args(["attach", source.path().to_str().unwrap()])
        .assert()
        .success();

    fs::write(
        workspace.path().join("report.docx"),
        docx("The quick brown fox leaps over the lazy dog."),
    )
    .unwrap();

    command(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success()
        .stdout(
            "M report.docx\n\
             -The quick brown fox jumps over the lazy dog.\n\
             +The quick brown fox leaps over the lazy dog.\n",
        );
}
