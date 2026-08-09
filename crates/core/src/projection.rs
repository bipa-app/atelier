use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use atelier_diff_core::PackageId;
use sha2::{Digest, Sha256};

use crate::engine::FileBlob;
use crate::error::Error;

const PROJECTIONS_DIR: &str = "projections";

/// Distinguishes concurrent publishers inside one process; the process id
/// distinguishes across processes.
static STAGED_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The derived-projection store under the control dir: never part of
/// history, keyed on (package name@version, blob content id). The package's
/// determinism contract (ADR-0003) makes an entry valid forever — a new
/// package version writes under a new key instead of invalidating.
pub(crate) struct ProjectionCache {
    dir: PathBuf,
}

impl ProjectionCache {
    pub fn new(control: &Path) -> Self {
        Self {
            dir: control.join(PROJECTIONS_DIR),
        }
    }

    /// The published projection of `blob` by `package`, when one exists.
    /// An entry is its text prefixed by the text's SHA-256 on one hex
    /// line; anything that cannot be read or fails the digest counts as a
    /// miss — the cache is derived, and the determinism contract makes the
    /// recomputed projection byte-identical, so reprojecting over a
    /// damaged entry heals it without changing any output. An actor who
    /// can write the control dir is out of scope: they hold the journal
    /// too.
    pub fn read(&self, package: PackageId, blob: &FileBlob) -> Option<String> {
        let entry = fs::read_to_string(self.entry(package, blob)).ok()?;
        let (digest, text) = entry.split_once('\n')?;
        if digest != hex_sha256(text) {
            return None;
        }
        Some(text.to_string())
    }

    /// Publish `text` as the projection of `blob` by `package`: written to
    /// a temp file unique to this publisher and renamed into place, so an
    /// interrupted write can never leave a partial entry for `read` to
    /// trust. Concurrent publishers race safely — determinism makes their
    /// contents identical.
    pub fn store(&self, package: PackageId, blob: &FileBlob, text: &str) -> Result<(), Error> {
        let entry = self.entry(package, blob);
        let parent = entry.parent().ok_or_else(|| {
            Error::Engine(format!(
                "projection entry {} has no parent",
                entry.display()
            ))
        })?;
        fs::create_dir_all(parent)?;
        let staged = entry.with_extension(format!(
            "staged-{}-{}",
            std::process::id(),
            STAGED_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&staged, format!("{}\n{text}", hex_sha256(text)))?;
        if let Err(error) = fs::rename(&staged, &entry) {
            // A concurrent publisher may have published this entry and
            // swept our staged file first; the published content is
            // identical, so the publication stands.
            if entry.is_file() {
                remove_if_present(&staged)?;
                return Ok(());
            }
            return Err(error.into());
        }
        self.sweep_staged(parent, &blob.id)
    }

    /// Remove staged files earlier publishers left behind for `blob` —
    /// crashes between write and rename orphan them. A complete entry is
    /// published by the time this runs, so a missing file only means a
    /// concurrent publisher finished the same cleanup.
    fn sweep_staged(&self, parent: &Path, blob_id: &str) -> Result<(), Error> {
        let staged_prefix = format!("{blob_id}.staged-");
        for sibling in fs::read_dir(parent)? {
            let sibling = sibling?;
            if sibling
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&staged_prefix))
            {
                remove_if_present(&sibling.path())?;
            }
        }
        Ok(())
    }

    fn entry(&self, package: PackageId, blob: &FileBlob) -> PathBuf {
        self.dir.join(package.to_string()).join(&blob.id)
    }
}

fn hex_sha256(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Remove `path`, treating its absence as done: a concurrent publisher
/// sweeping the same staged files legitimately gets there first.
fn remove_if_present(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
