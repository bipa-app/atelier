//! The jj/git store plane (ADR-0013 §3, H4): engine stores are
//! content-addressed files, so they replicate as blob uploads under the
//! workspace's shared `objects/` prefix plus one manifest — the head
//! pointer — written under the held epoch. A lineage without a manifest
//! never completed a pass, and no hydration selects it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ownership::Ownership;
use crate::{HostedError, hosted_err};

/// The key one pass's manifest lives under, inside its epoch's lineage.
pub(crate) const HEADS_KEY: &str = "stores";

/// One file a manifest pins: its content id and its mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHead {
    /// The SHA-256 of the file's bytes — its key under `objects/`.
    pub content: String,
    /// Whether the file carries the executable bit (adopted repos ship
    /// hooks; dropping the bit would break them silently).
    pub executable: bool,
}

/// The head pointer one replication pass writes under its epoch: the
/// `SQLite` transaction the pass captured, every engine store's exact
/// files by content id, and the workspace config. Hydration restores the
/// `SQLite` store point-in-time to `txid`, so both stores come from one
/// completed pass — never a journal naming snapshots the stores lack.
/// The store map is keyed by mount name; the root store is `""`, a name
/// no mount can take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreHeads {
    /// The newest `SQLite` transaction in the lineage when the pass
    /// captured — what hydration restores to.
    pub txid: u64,
    /// The workspace's `.atelier/config`, when the root is a workspace.
    pub config: Option<FileHead>,
    /// Store name → store-relative path → file head.
    pub stores: BTreeMap<String, BTreeMap<String, FileHead>>,
}

/// Capture the workspace's engine stores under `epoch`: upload the blobs
/// the bucket lacks, then pin the manifest. `known` carries the content
/// ids already uploaded, seeded from the bucket at activation.
pub(crate) fn capture(
    root: &Path,
    ownership: &Ownership,
    epoch: u64,
    txid: u64,
    known: &mut BTreeSet<String>,
) -> Result<(), HostedError> {
    let mut heads = StoreHeads {
        txid,
        config: None,
        stores: BTreeMap::new(),
    };
    let config = root.join(".atelier").join("config.toml");
    if config.is_file() {
        heads.config = Some(upload(&config, ownership, known)?);
    }
    for (name, dir) in store_roots(root)? {
        let mut files = BTreeMap::new();
        for relative in store_files(&dir)? {
            let head = upload(&dir.join(&relative), ownership, known)?;
            files.insert(relative, head);
        }
        heads.stores.insert(name, files);
    }
    let manifest = serde_json::to_vec(&heads).map_err(hosted_err)?;
    ownership.put_under_epoch(epoch, HEADS_KEY, manifest)
}

/// The newest epoch below `below` whose pass completed — its manifest,
/// parsed. Hydration restores both the `SQLite` store and the engine
/// stores from this one lineage, so they never mix epochs.
pub(crate) fn newest_heads(
    ownership: &Ownership,
    below: u64,
) -> Result<Option<(u64, StoreHeads)>, HostedError> {
    for prior in (1..below).rev() {
        if let Some(bytes) = ownership.get_under_epoch(prior, HEADS_KEY)? {
            let heads: StoreHeads = serde_json::from_slice(&bytes).map_err(hosted_err)?;
            return Ok(Some((prior, heads)));
        }
    }
    Ok(None)
}

/// Rebuild the engine stores a manifest pins into `root`. Every write is
/// create-new: a colliding file means the root already holds state, and
/// overwriting it would fabricate history.
pub(crate) fn hydrate(
    root: &Path,
    ownership: &Ownership,
    heads: &StoreHeads,
) -> Result<(), HostedError> {
    if let Some(config) = &heads.config {
        place(
            &root.join(".atelier").join("config.toml"),
            config,
            ownership,
        )?;
    }
    for (name, files) in &heads.stores {
        let dir = if name.is_empty() {
            root.to_path_buf()
        } else {
            root.join(name)
        };
        for (relative, head) in files {
            place(&dir.join(relative), head, ownership)?;
        }
    }
    Ok(())
}

/// Upload one file's bytes when the bucket lacks them; its head either way.
fn upload(
    path: &Path,
    ownership: &Ownership,
    known: &mut BTreeSet<String>,
) -> Result<FileHead, HostedError> {
    let bytes = fs::read(path).map_err(hosted_err)?;
    let executable = fs::metadata(path).map_err(hosted_err)?.permissions().mode() & 0o111 != 0;
    let content = format!("{:x}", Sha256::digest(&bytes));
    if !known.contains(&content) {
        ownership.put_object(&content, bytes)?;
        known.insert(content.clone());
    }
    Ok(FileHead {
        content,
        executable,
    })
}

/// Download one head's blob and write it at `path`.
fn place(path: &Path, head: &FileHead, ownership: &Ownership) -> Result<(), HostedError> {
    let bytes = ownership.get_object(&head.content)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(hosted_err)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(path).map_err(hosted_err)?;
    std::io::Write::write_all(&mut file, &bytes).map_err(hosted_err)?;
    if head.executable {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(hosted_err)?;
    }
    Ok(())
}

/// The engine stores under `root`, by structure: the root's own when a
/// `.jj` sits at the root, and every direct child directory carrying one —
/// the shape mounted sources have. Discovery reads the tree, not the
/// workspace config, so the hosted crate never parses core formats.
fn store_roots(root: &Path) -> Result<Vec<(String, PathBuf)>, HostedError> {
    let mut roots = Vec::new();
    if root.join(".jj").is_dir() {
        roots.push((String::new(), root.to_path_buf()));
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(root).map_err(hosted_err)? {
        let entry = entry.map_err(hosted_err)?;
        // A non-utf8 name cannot be a mount: mount names are utf-8 by
        // construction, so this is structure outside any store.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if matches!(name.as_str(), ".atelier" | ".jj" | ".git") {
            continue;
        }
        let dir = entry.path();
        if dir.is_dir() && dir.join(".jj").is_dir() {
            children.push((name, dir));
        }
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    roots.extend(children);
    Ok(roots)
}

/// Every replicable file in one store, relative to its directory: the
/// whole `.jj` and `.git` trees minus derived per-machine state — the
/// working copy rematerializes from history (ADR-0013), and git's index
/// is working-tree state. A symlink refuses: replicating its target
/// would fabricate content, and dropping it would shorten the store.
fn store_files(dir: &Path) -> Result<Vec<String>, HostedError> {
    let mut files = Vec::new();
    let mut stack = vec![dir.join(".jj")];
    if dir.join(".git").is_dir() {
        stack.push(dir.join(".git"));
    }
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).map_err(hosted_err)? {
            let entry = entry.map_err(hosted_err)?;
            let path = entry.path();
            let kind = entry.file_type().map_err(hosted_err)?;
            let relative = relative_to(dir, &path)?;
            if relative == ".jj/working_copy" || relative == ".git/index" {
                continue;
            }
            if kind.is_symlink() {
                return Err(HostedError(format!(
                    "store file {relative} is a symlink; stores replicate regular files only"
                )));
            }
            if kind.is_dir() {
                stack.push(path);
            } else {
                files.push(relative);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// A store file's path relative to its store directory, slash-separated.
fn relative_to(dir: &Path, path: &Path) -> Result<String, HostedError> {
    let relative = path
        .strip_prefix(dir)
        .map_err(|_| HostedError(format!("{} escaped its store", path.display())))?;
    match relative.to_str() {
        Some(text) => Ok(text.to_owned()),
        None => Err(HostedError(format!(
            "store path {} is not utf-8",
            relative.display()
        ))),
    }
}
