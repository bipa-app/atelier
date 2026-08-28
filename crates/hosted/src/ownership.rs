//! Ownership records and fencing epochs (ADR-0013, H2 — celld's design):
//! one record per workspace, written conditionally; every activation
//! advances the epoch; the data path writes plainly under epoch-prefixed
//! keys, so a deposed writer lands in a superseded lineage; an
//! acknowledgement re-reads the record, so a stale node cannot make a
//! promise the surviving lineage does not keep. A release clears the
//! holder but keeps the record: the epoch is a high-water mark no
//! activation ever reuses.

use std::collections::BTreeSet;
use std::path::Path as FsPath;
use std::sync::Arc;

use futures::StreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::{HostedError, hosted_err};

/// The one ownership record a workspace has in the bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipRecord {
    /// The node session that holds the workspace; a released workspace
    /// keeps its record with no holder, preserving the epoch.
    pub holder: Option<String>,
    /// The fencing epoch: advanced on every activation, never reused —
    /// an epoch has exactly one writer, ever.
    pub epoch: u64,
}

/// What one claim attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The record now names this holder at this epoch.
    Held {
        /// The epoch this holder now writes under.
        epoch: u64,
    },
    /// Another holder owns the workspace; a plain claim never seizes.
    HeldByOther {
        /// The node session that holds the workspace.
        holder: String,
        /// The epoch the holder writes under.
        epoch: u64,
    },
}

/// What one release attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The record now names no holder; the epoch stays as the high-water
    /// mark the next activation advances past.
    Released,
    /// The record does not name this holder at this epoch — the workspace
    /// moved on and the release is moot.
    NotHeld,
}

/// A workspace's ownership plane in the bucket: the record and the
/// epoch-prefixed data path beneath one workspace prefix.
pub struct Ownership {
    store: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
    runtime: Runtime,
}

impl Ownership {
    /// Open the ownership plane over an already-built store — the shape a
    /// hosted node uses when it shares one store across workspaces.
    pub fn from_store(
        store: Arc<dyn ObjectStore>,
        prefix: ObjectPath,
    ) -> Result<Self, HostedError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(hosted_err)?;
        Ok(Self {
            store,
            prefix,
            runtime,
        })
    }

    /// Claim the workspace for `holder`: a conditional create when no
    /// record exists, a compare-and-swap over a released record or this
    /// holder's own on re-activation — either way the epoch advances. A
    /// record naming another holder refuses; seizing is `take_over`, a
    /// deliberate act.
    pub fn claim(&self, holder: &str) -> Result<ClaimOutcome, HostedError> {
        let current = self.record_with_version()?;
        match &current {
            Some((record, _)) => match &record.holder {
                Some(other) if other != holder => Ok(ClaimOutcome::HeldByOther {
                    holder: other.clone(),
                    epoch: record.epoch,
                }),
                Some(_) | None => self.advance(holder, current),
            },
            None => self.advance(holder, current),
        }
    }

    /// Seize the workspace for `holder` regardless of the current record:
    /// the epoch advances, so the deposed writer's lineage is superseded —
    /// its late writes land under a prefix no restore selects.
    pub fn take_over(&self, holder: &str) -> Result<ClaimOutcome, HostedError> {
        let current = self.record_with_version()?;
        self.advance(holder, current)
    }

    /// Release the workspace: a guarded write that clears the holder and
    /// keeps the epoch as the high-water mark. A record that no longer
    /// names `holder` at `epoch` refuses — the workspace moved on and the
    /// release is moot.
    pub fn release(&self, holder: &str, epoch: u64) -> Result<ReleaseOutcome, HostedError> {
        let Some((record, version)) = self.record_with_version()? else {
            return Ok(ReleaseOutcome::NotHeld);
        };
        if record.holder.as_deref() != Some(holder) {
            return Ok(ReleaseOutcome::NotHeld);
        }
        if record.epoch != epoch {
            return Ok(ReleaseOutcome::NotHeld);
        }
        let released = OwnershipRecord {
            holder: None,
            epoch,
        };
        if self.write_record(&released, PutMode::Update(version))? {
            Ok(ReleaseOutcome::Released)
        } else {
            Ok(ReleaseOutcome::NotHeld)
        }
    }

    /// The acknowledgement rule: re-read the record and answer whether it
    /// still names `holder` at `epoch`. A paused or partitioned node fails
    /// this read; no clock is consulted.
    pub fn confirm(&self, holder: &str, epoch: u64) -> Result<bool, HostedError> {
        Ok(self.record_with_version()?.is_some_and(|(record, _)| {
            record.holder.as_deref() == Some(holder) && record.epoch == epoch
        }))
    }

    /// The current record, if any — holder-bearing while a node serves,
    /// holderless after a release, absent before the first claim.
    pub fn record(&self) -> Result<Option<OwnershipRecord>, HostedError> {
        Ok(self.record_with_version()?.map(|(record, _)| record))
    }

    /// A plain data-path write under `epoch`'s lineage: the fence is the
    /// epoch in the key, not a condition on the request.
    pub fn put_under_epoch(
        &self,
        epoch: u64,
        key: &str,
        bytes: Vec<u8>,
    ) -> Result<(), HostedError> {
        let location = self.epoch_path(epoch, key);
        self.runtime
            .block_on(
                self.store
                    .put(&location, PutPayload::from_bytes(bytes.into())),
            )
            .map_err(hosted_err)?;
        Ok(())
    }

    /// One key under `epoch`'s lineage, if a pass wrote it.
    pub fn get_under_epoch(&self, epoch: u64, key: &str) -> Result<Option<Vec<u8>>, HostedError> {
        let location = self.epoch_path(epoch, key);
        self.runtime.block_on(async {
            match self.store.get(&location).await {
                Ok(result) => Ok(Some(result.bytes().await.map_err(hosted_err)?.to_vec())),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(error) => Err(hosted_err(error)),
            }
        })
    }

    /// A content-addressed blob write under the workspace's shared
    /// `objects/` prefix: the key is the content id, so every epoch's
    /// manifest can name it and a re-upload is a no-op by construction.
    pub fn put_object(&self, content_id: &str, bytes: Vec<u8>) -> Result<(), HostedError> {
        let location = self.object_path(content_id);
        self.runtime
            .block_on(
                self.store
                    .put(&location, PutPayload::from_bytes(bytes.into())),
            )
            .map_err(hosted_err)?;
        Ok(())
    }

    /// A content-addressed blob a manifest promised. Absence is an error:
    /// manifests are written after their blobs, so a missing one means
    /// the bucket lost data.
    pub fn get_object(&self, content_id: &str) -> Result<Vec<u8>, HostedError> {
        let location = self.object_path(content_id);
        self.runtime.block_on(async {
            match self.store.get(&location).await {
                Ok(result) => Ok(result.bytes().await.map_err(hosted_err)?.to_vec()),
                Err(object_store::Error::NotFound { .. }) => Err(HostedError(format!(
                    "the bucket lost object {content_id}: a manifest names it"
                ))),
                Err(error) => Err(hosted_err(error)),
            }
        })
    }

    /// Every content id under the shared `objects/` prefix — what uploads
    /// dedupe against.
    pub fn objects(&self) -> Result<BTreeSet<String>, HostedError> {
        let prefix = ObjectPath::from(format!("{}/objects", self.prefix));
        self.runtime.block_on(async {
            let mut stream = self.store.list(Some(&prefix));
            let mut ids = BTreeSet::new();
            while let Some(meta) = stream.next().await {
                let meta = meta.map_err(hosted_err)?;
                if let Some(id) = meta.location.filename() {
                    ids.insert(id.to_owned());
                }
            }
            Ok(ids)
        })
    }

    /// Every key under `epoch`'s lineage — what a restore of that lineage
    /// would consider, and nothing from any other epoch.
    pub fn keys_under_epoch(&self, epoch: u64) -> Result<Vec<String>, HostedError> {
        let prefix = self.epoch_path(epoch, "");
        self.runtime.block_on(async {
            let mut stream = self.store.list(Some(&prefix));
            let mut keys = Vec::new();
            while let Some(meta) = stream.next().await {
                let meta = meta.map_err(hosted_err)?;
                keys.push(meta.location.to_string());
            }
            keys.sort();
            Ok(keys)
        })
    }

    fn epoch_path(&self, epoch: u64, key: &str) -> ObjectPath {
        let lineage = format!("{}/ltx/e{epoch}/{key}", self.prefix);
        ObjectPath::from(lineage.trim_end_matches('/').to_owned())
    }

    fn object_path(&self, content_id: &str) -> ObjectPath {
        ObjectPath::from(format!("{}/objects/{content_id}", self.prefix))
    }

    fn record_path(&self) -> ObjectPath {
        ObjectPath::from(format!("{}/ownership", self.prefix))
    }

    /// The current record with the version a swap must name.
    fn record_with_version(&self) -> Result<Option<(OwnershipRecord, UpdateVersion)>, HostedError> {
        let location = self.record_path();
        self.runtime.block_on(async {
            match self.store.get(&location).await {
                Ok(result) => {
                    let version = UpdateVersion {
                        e_tag: result.meta.e_tag.clone(),
                        version: result.meta.version.clone(),
                    };
                    let bytes = result.bytes().await.map_err(hosted_err)?;
                    let record: OwnershipRecord =
                        serde_json::from_slice(&bytes).map_err(hosted_err)?;
                    Ok(Some((record, version)))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(error) => Err(hosted_err(error)),
            }
        })
    }

    /// Write the advanced record conditionally: create when absent, swap
    /// naming the exact version read. The bucket accepts one such write,
    /// so two nodes cannot advance over the same record.
    fn advance(
        &self,
        holder: &str,
        current: Option<(OwnershipRecord, UpdateVersion)>,
    ) -> Result<ClaimOutcome, HostedError> {
        let epoch = current.as_ref().map_or(1, |(record, _)| record.epoch + 1);
        let record = OwnershipRecord {
            holder: Some(holder.to_owned()),
            epoch,
        };
        let mode = match current {
            None => PutMode::Create,
            Some((_, version)) => PutMode::Update(version),
        };
        if self.write_record(&record, mode)? {
            return Ok(ClaimOutcome::Held { epoch });
        }
        // Lost the race: whoever won holds the point; say who.
        match self.record_with_version()? {
            Some((record, _)) => match record.holder {
                Some(holder) => Ok(ClaimOutcome::HeldByOther {
                    holder,
                    epoch: record.epoch,
                }),
                None => Err(HostedError(
                    "the ownership write lost a race to a release; claim again".to_owned(),
                )),
            },
            None => Err(HostedError(
                "the ownership write lost a race to a record that then vanished".to_owned(),
            )),
        }
    }

    /// One conditional write of the record: true when the bucket accepted
    /// it, false when another writer moved the record first.
    fn write_record(&self, record: &OwnershipRecord, mode: PutMode) -> Result<bool, HostedError> {
        let payload = serde_json::to_vec(record).map_err(hosted_err)?;
        let options = PutOptions {
            mode,
            ..PutOptions::default()
        };
        let written = self.runtime.block_on(self.store.put_opts(
            &self.record_path(),
            PutPayload::from_bytes(payload.into()),
            options,
        ));
        match written {
            Ok(_) => Ok(true),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => Ok(false),
            Err(error) => Err(hosted_err(error)),
        }
    }
}

/// A workspace prefix as a URL for a local directory — the file:// scheme
/// tests every code path real buckets take.
#[must_use]
pub fn file_url(dir: &FsPath) -> String {
    format!("file://{}", dir.display())
}
