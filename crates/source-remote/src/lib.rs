//! The remote source adapter (ADR-0012): one seam over `object_store`
//! speaking s3://, gs://, az://, and file:// — the last making every code
//! path testable without a network. The adapter owns a contained
//! current-thread tokio runtime and exposes blocking functions, so the
//! core's execution model stays synchronous.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path as FsPath;
use std::sync::Arc;

use futures::StreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;
use url::Url;

/// The names a mirror never uploads and a download never writes: engine
/// internals stay on the machine.
const SKIP_NAMES: [&str; 3] = [".atelier", ".jj", ".git"];

/// One failure in the adapter, rendered for the workspace's error surface.
#[derive(Debug)]
pub struct RemoteError(String);

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RemoteError {}

fn remote_err(source: impl fmt::Display) -> RemoteError {
    RemoteError(source.to_string())
}

/// Whether a source path is a remote URL this adapter speaks.
#[must_use]
pub fn is_remote_url(source: &str) -> bool {
    [
        "s3://",
        "gs://",
        "az://",
        "azure://",
        "file://",
        "memory://",
    ]
    .iter()
    .any(|scheme| source.starts_with(scheme))
}

/// An attached bucket prefix: the store, the prefix, and the runtime that
/// drives it — opened once per operation, never held across them.
pub struct RemoteFolder {
    store: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
    runtime: Runtime,
}

impl RemoteFolder {
    /// Open the URL's store. Credentials come from the environment (the
    /// provider's own variables); nothing is persisted.
    pub fn open(source: &str) -> Result<Self, RemoteError> {
        let url = Url::parse(source).map_err(remote_err)?;
        let (store, prefix) = object_store::parse_url(&url).map_err(remote_err)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(remote_err)?;
        Ok(Self {
            store: Arc::from(store),
            prefix,
            runtime,
        })
    }

    /// Download every object under the prefix into `target`, keys becoming
    /// relative paths. Engine-internal names refuse: a bucket must not
    /// smuggle a repository into the mount.
    pub fn download_all(&self, target: &FsPath) -> Result<(), RemoteError> {
        self.runtime.block_on(async {
            let keys = self.keys().await?;
            for key in keys {
                let object_path = self.object_path(&key);
                let bytes = self
                    .store
                    .get(&object_path)
                    .await
                    .map_err(remote_err)?
                    .bytes()
                    .await
                    .map_err(remote_err)?;
                let file = target.join(&key);
                if let Some(parent) = file.parent() {
                    fs::create_dir_all(parent).map_err(remote_err)?;
                }
                fs::write(&file, &bytes).map_err(remote_err)?;
            }
            Ok(())
        })
    }

    /// A digest of the listing — every key with its `ETag` and size, sorted.
    /// Two listings digest alike exactly when a mirror would find nothing
    /// to reconcile (ADR-0012: the `ETag` stands in for content bytes).
    pub fn fingerprint(&self) -> Result<String, RemoteError> {
        self.runtime.block_on(async {
            let mut entries = self.listing().await?;
            entries.sort();
            let mut hasher = Sha256::new();
            for (key, etag, size) in &entries {
                hasher.update(key.as_bytes());
                hasher.update([0]);
                hasher.update(etag.as_bytes());
                hasher.update([0]);
                hasher.update(size.to_le_bytes());
            }
            Ok(format!("{:x}", hasher.finalize()))
        })
    }

    /// Reconcile the bucket against `source_dir` as a mirror: upload added
    /// and changed objects, delete removed ones. Engine-internal names
    /// never travel. The caller holds the fingerprint guard.
    pub fn mirror(&self, source_dir: &FsPath) -> Result<(), RemoteError> {
        let local = local_files(source_dir)?;
        self.runtime.block_on(async {
            let remote = self.keys().await?;
            for key in &local {
                let bytes = fs::read(source_dir.join(key)).map_err(remote_err)?;
                self.store
                    .put(&self.object_path(key), PutPayload::from_bytes(bytes.into()))
                    .await
                    .map_err(remote_err)?;
            }
            for key in remote.difference(&local) {
                self.store
                    .delete(&self.object_path(key))
                    .await
                    .map_err(remote_err)?;
            }
            Ok(())
        })
    }

    /// Mirror the bucket into `target`: download every object, then remove
    /// local files the listing lacks and prune emptied directories.
    /// Engine-internal names never travel in either direction.
    pub fn download_mirror(&self, target: &FsPath) -> Result<(), RemoteError> {
        self.download_all(target)?;
        let remote = self.runtime.block_on(self.keys())?;
        let local = local_files(target)?;
        let mut directories = BTreeSet::new();
        for stale in local.difference(&remote) {
            let path = target.join(stale);
            fs::remove_file(&path).map_err(remote_err)?;
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if dir == target {
                    break;
                }
                directories.insert(dir.to_path_buf());
                parent = dir.parent();
            }
        }
        // Deepest first, so an emptied child empties its parent in turn.
        let mut directories: Vec<_> = directories.into_iter().collect();
        directories.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
        for dir in directories {
            if fs::read_dir(&dir).map_err(remote_err)?.next().is_none() {
                fs::remove_dir(&dir).map_err(remote_err)?;
            }
        }
        Ok(())
    }

    /// The object path a key names beneath the prefix.
    fn object_path(&self, key: &str) -> ObjectPath {
        if self.prefix.as_ref().is_empty() {
            ObjectPath::from(key)
        } else {
            ObjectPath::from(format!("{}/{key}", self.prefix))
        }
    }

    /// Every key under the prefix, relative to it.
    async fn keys(&self) -> Result<BTreeSet<String>, RemoteError> {
        Ok(self
            .listing()
            .await?
            .into_iter()
            .map(|(key, _, _)| key)
            .collect())
    }

    async fn listing(&self) -> Result<Vec<(String, String, u64)>, RemoteError> {
        let mut stream = self.store.list(Some(&self.prefix));
        let mut entries = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(remote_err)?;
            let Some(key) = relative_key(&self.prefix, &meta.location) else {
                continue;
            };
            if key
                .split('/')
                .any(|component| SKIP_NAMES.contains(&component))
            {
                return Err(RemoteError(format!(
                    "the bucket carries an engine-internal name at {key:?}; refusing to import it"
                )));
            }
            let etag = meta.e_tag.unwrap_or_else(|| {
                // Stores without ETags (file://) still guard: the
                // modification time and size stand in.
                format!("mtime:{}", meta.last_modified.timestamp_micros())
            });
            entries.push((key, etag, meta.size));
        }
        Ok(entries)
    }
}

/// A key relative to the prefix; `None` for the prefix itself.
fn relative_key(prefix: &ObjectPath, location: &ObjectPath) -> Option<String> {
    let parts: Vec<String> = location
        .prefix_match(prefix)?
        .map(|part| part.as_ref().to_owned())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Every file under `dir` as a bucket key, engine internals skipped at any
/// depth; an explicit work stack bounds the walk by entry count.
fn local_files(dir: &FsPath) -> Result<BTreeSet<String>, RemoteError> {
    let mut keys = BTreeSet::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).map_err(remote_err)? {
            let entry = entry.map_err(remote_err)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(RemoteError(format!(
                    "cannot mirror a non-utf8 name at {}",
                    entry.path().display()
                )));
            };
            if SKIP_NAMES.contains(&name) {
                continue;
            }
            let path = entry.path();
            if entry.file_type().map_err(remote_err)?.is_dir() {
                pending.push(path);
            } else {
                let key = path
                    .strip_prefix(dir)
                    .map_err(remote_err)?
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                keys.insert(key);
            }
        }
    }
    Ok(keys)
}
