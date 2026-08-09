use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use atelier_core::{Act, ActorKind, DeltaKind, Error, Fidelity, LineKind, Workspace};

/// Serialize tests: they all set the process-wide `ATELIER_CONFIG_HOME`.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_actor(config_home: &Path) {
    fs::create_dir_all(config_home).unwrap();
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .unwrap();
    // SAFETY: every test holds `env_lock()` for its whole body, so no other
    // thread reads or writes the environment concurrently.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

fn point_config_home(config_home: &Path) {
    // SAFETY: as above; guarded by `env_lock()`.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

#[test]
fn init_creates_control_dir_journal_and_git() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();

    assert!(root.path().join(".atelier/config.toml").is_file());
    assert!(root.path().join(".atelier/journal.sqlite3").is_file());
    assert!(root.path().join(".git").exists());

    let entries = ws.journal(10).unwrap();
    assert!(entries.iter().any(|entry| {
        entry.act == Act::WorkspaceInit
            && entry.actor_name == "test-actor"
            && entry.actor_kind == ActorKind::Human
    }));
}

#[test]
fn attach_imports_files_and_records_snapshot() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), "hi").unwrap();
    fs::create_dir(source.path().join("sub")).unwrap();
    fs::write(source.path().join("sub/nested.txt"), "nested").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    assert!(root.path().join("hello.txt").is_file());
    assert!(root.path().join("sub/nested.txt").is_file());

    let entries = ws.journal(20).unwrap();
    assert!(entries.iter().any(|entry| entry.act == Act::SourceAttach));

    let log = ws.log(20).unwrap();
    assert!(!log.is_empty());
    assert!(log.iter().all(|snapshot| snapshot.actor == "test-actor"));
}

#[test]
fn edit_of_text_file_raises_changed_delta_to_text_rung() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), "hi").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("hello.txt"), "changed").unwrap();

    let entries = ws.journal(50).unwrap();
    assert!(entries.iter().any(|entry| entry.act == Act::Snapshot));

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "hello.txt");
    assert_eq!(diff.deltas[0].kind, DeltaKind::Changed);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Text);
    assert!(
        diff.deltas[0]
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Removed && line.text == "hi")
    );
    assert!(
        diff.deltas[0]
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Added && line.text == "changed")
    );
}

#[test]
fn edit_of_markdown_file_raises_changed_delta_to_text_rung() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("notes.md"), "# Title\n\nfirst draft\n").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("notes.md"), "# Title\n\nsecond draft\n").unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "notes.md");
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Text);
    assert_eq!(diff.deltas[0].package, None);
    assert!(
        diff.deltas[0]
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Added && line.text == "second draft")
    );
}

#[test]
fn edit_of_unknown_format_file_stays_at_binary_rung() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("opaque.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("opaque.bin"), [0x00, 0xff, 0x02, 0x03]).unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "opaque.bin");
    assert_eq!(diff.deltas[0].kind, DeltaKind::Changed);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Binary);
    assert!(diff.deltas[0].lines.is_empty());
}

/// A one-paragraph Word document holding `sentence`, zipped as a .docx.
fn docx(sentence: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{sentence}</w:t></w:r></w:p></w:body></w:document>"#
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("word/document.xml", options).unwrap();
    writer.write_all(document.as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

#[test]
fn changed_docx_raises_to_text_rung_carrying_its_package_and_reuses_the_cache() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("report.docx"), docx("first version")).unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("report.docx"), docx("second version")).unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Text);
    assert_eq!(
        diff.deltas[0].package.map(|package| package.to_string()),
        Some("format-docx@0.1.0".to_string())
    );

    // The second diff of the same snapshots serves both sides from the
    // published cache entries and must carry the identical comparison.
    let again = ws.diff_latest().unwrap();
    assert_eq!(again, diff);
}

#[test]
fn file_past_the_ladder_cap_stays_binary_and_journals_the_degradation() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    // 9 MiB of text: past the 8 MiB ladder cap, under the snapshot limit.
    let big = "all work and no play makes agents dull\n".repeat(9 * 1024 * 1024 / 39);
    fs::write(source.path().join("huge.txt"), &big).unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("huge.txt"), format!("{big}changed\n")).unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Binary);
    assert!(diff.deltas[0].lines.is_empty());

    let entries = ws.journal(50).unwrap();
    let capped = entries
        .iter()
        .find(|entry| entry.act == Act::FileTooLarge)
        .expect("the degradation must be journaled");
    let reference = capped.reference.as_deref().expect("reference names it");
    assert!(reference.contains("huge.txt"), "got: {reference}");
    assert!(reference.contains("ladder cap"), "got: {reference}");
}

#[test]
fn docx_without_zip_magic_is_claimed_and_its_failure_journaled() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    // Plain ASCII masquerading as a .docx: the package must claim it by
    // extension and fail loudly, never let it diff as plain text.
    fs::write(source.path().join("fake.docx"), "just some text").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("fake.docx"), "just some text, changed").unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Binary);
    assert!(diff.deltas[0].lines.is_empty());

    let entries = ws.journal(50).unwrap();
    assert!(
        entries.iter().any(|entry| entry.act == Act::PackageFailed),
        "the extension claim must journal the projection failure"
    );
}

#[test]
fn a_corrupted_cache_entry_heals_on_the_next_diff() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("report.docx"), docx("first version")).unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(root.path().join("report.docx"), docx("second version")).unwrap();
    let before = ws.diff_latest().unwrap();

    // Corrupt every published projection entry with invalid UTF-8; the
    // next diff must treat them as misses and reproject identically.
    let projections = root.path().join(".atelier/projections");
    for package_dir in fs::read_dir(&projections).unwrap() {
        for entry in fs::read_dir(package_dir.unwrap().path()).unwrap() {
            fs::write(entry.unwrap().path(), [0xff, 0xfe, 0xff]).unwrap();
        }
    }

    let after = ws.diff_latest().unwrap();
    assert_eq!(after, before);

    // Forged valid-UTF-8 entries fail the digest and heal the same way.
    for package_dir in fs::read_dir(&projections).unwrap() {
        for entry in fs::read_dir(package_dir.unwrap().path()).unwrap() {
            fs::write(entry.unwrap().path(), "forged cache value\n").unwrap();
        }
    }

    let healed = ws.diff_latest().unwrap();
    assert_eq!(healed, before);
}

#[test]
fn an_unwritable_projection_cache_does_not_gate_the_diff() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("report.docx"), docx("first version")).unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    // Block the projections directory with a plain file: publishing fails,
    // but the projection is already computed — the diff must still raise.
    fs::write(root.path().join(".atelier/projections"), "in the way").unwrap();
    fs::write(root.path().join("report.docx"), docx("second version")).unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Text);
}

#[test]
fn failing_package_falls_back_to_binary_and_journals_the_failure() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    // Zip magic makes the docx package claim it; the broken archive then
    // fails projection.
    fs::write(
        source.path().join("broken.docx"),
        b"PK\x03\x04 not a real zip",
    )
    .unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    fs::write(
        root.path().join("broken.docx"),
        b"PK\x03\x04 still not a real zip",
    )
    .unwrap();

    let diff = ws.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].fidelity, Fidelity::Binary);
    assert!(diff.deltas[0].lines.is_empty());

    let entries = ws.journal(50).unwrap();
    let failure = entries
        .iter()
        .find(|entry| entry.act == Act::PackageFailed)
        .expect("the fallback must be journaled");
    let reference = failure.reference.as_deref().expect("reference names it");
    assert!(reference.contains("broken.docx"), "got: {reference}");
    assert!(reference.contains("format-docx@0.1.0"), "got: {reference}");
    assert!(
        reference.contains("fell_back_to=binary"),
        "got: {reference}"
    );
}

#[test]
fn missing_actor_config_is_reported() {
    let _guard = env_lock();
    let empty = tempfile::tempdir().unwrap();
    point_config_home(empty.path());
    let root = tempfile::tempdir().unwrap();

    let result = Workspace::init(root.path());

    assert!(matches!(result, Err(Error::NoActorConfigured)));
}

#[test]
fn attaching_an_lfs_source_is_rejected() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(
        source.path().join(".gitattributes"),
        "*.psd filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    let err = ws.attach(source.path()).unwrap_err();

    assert!(matches!(err, Error::LfsSourceUnsupported));
}

#[test]
fn boundary_paths_never_appear_in_deltas_or_log() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), "hi").unwrap();

    let mut ws = Workspace::init(root.path()).unwrap();
    ws.attach(source.path()).unwrap();

    let diff = ws.diff_latest().unwrap();
    assert!(!diff.deltas.is_empty());
    for delta in &diff.deltas {
        let address = delta.address.as_str();
        assert!(!address.starts_with(".atelier"), "leaked: {address}");
        assert!(!address.starts_with(".jj"), "leaked: {address}");
        assert!(!address.starts_with(".git"), "leaked: {address}");
    }

    for snapshot in ws.log(50).unwrap() {
        for parent in &snapshot.parents {
            assert!(!parent.is_empty());
        }
    }
}
