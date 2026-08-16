//! Ownership records and fencing epochs (ADR-0013, H2 — celld's design):
//! one record per workspace, written conditionally; every activation
//! advances the epoch; the data path writes plainly under epoch-prefixed
//! keys, so a deposed writer lands in a superseded lineage; an
//! acknowledgement re-reads the record, so a stale node cannot make a
//! promise the surviving lineage does not keep.

use std::path::Path as FsPath;
use std::sync::Arc;

use futures::StreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use url::Url;

use crate::{HostedError, hosted_err};

/// The one ownership record a workspace has in the bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipRecord {
    /// The node session that owns the workspace.
    pub holder: String,
    /// The fencing epoch: advanced on every activation, never reused —
    /// an epoch has exactly one writer, ever.
    pub epoch: u64,
}

/// What one claim attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The record now names this holder at this epoch.
    Held { epoch: u64 },
    /// Another holder owns the workspace; a plain claim never seizes.
    HeldByOther { holder: String, epoch: u64 },
}

/// A workspace's ownership plane in the bucket: the record and the
/// epoch-prefixed data path beneath one workspace prefix.
pub struct Ownership {
    store: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
    runtime: Runtime,
}

impl Ownership {
    /// Open the workspace prefix at `url` (s3://, gs://, az://).
    /// Credentials come from the environment. The ownership record needs
    /// conditional writes: `LocalFileSystem` does not implement them, so
    /// file:// refuses here — tests share an in-memory store instead.
    pub fn open(url: &str) -> Result<Self, HostedError> {
        let url = Url::parse(url).map_err(hosted_err)?;
        let (store, prefix) = object_store::parse_url(&url).map_err(hosted_err)?;
        Self::from_store(Arc::from(store), prefix)
    }

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
    /// record exists, a compare-and-swap over this holder's own record on
    /// re-activation — either way the epoch advances. A record naming
    /// another holder refuses; seizing is `take_over`, a deliberate act.
    pub fn claim(&self, holder: &str) -> Result<ClaimOutcome, HostedError> {
        let current = self.record()?;
        match &current {
            Some((record, _)) if record.holder != holder => Ok(ClaimOutcome::HeldByOther {
                holder: record.holder.clone(),
                epoch: record.epoch,
            }),
            _ => self.advance(holder, current),
        }
    }

    /// Seize the workspace for `holder` regardless of the current record:
    /// the epoch advances, so the deposed writer's lineage is superseded —
    /// its late writes land under a prefix no restore selects.
    pub fn take_over(&self, holder: &str) -> Result<ClaimOutcome, HostedError> {
        let current = self.record()?;
        self.advance(holder, current)
    }

    /// The acknowledgement rule: re-read the record and answer whether it
    /// still names `holder` at `epoch`. A paused or partitioned node fails
    /// this read; no clock is consulted.
    pub fn confirm(&self, holder: &str, epoch: u64) -> Result<bool, HostedError> {
        Ok(self
            .record()?
            .is_some_and(|(record, _)| record.holder == holder && record.epoch == epoch))
    }

    /// The current record, if any, with the version a swap must name.
    pub fn holder(&self) -> Result<Option<OwnershipRecord>, HostedError> {
        Ok(self.record()?.map(|(record, _)| record))
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

    fn record(&self) -> Result<Option<(OwnershipRecord, UpdateVersion)>, HostedError> {
        let location = ObjectPath::from(format!("{}/ownership", self.prefix));
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
            holder: holder.to_owned(),
            epoch,
        };
        let mode = match current {
            None => PutMode::Create,
            Some((_, version)) => PutMode::Update(version),
        };
        let location = ObjectPath::from(format!("{}/ownership", self.prefix));
        let payload = serde_json::to_vec(&record).map_err(hosted_err)?;
        let options = PutOptions {
            mode,
            ..PutOptions::default()
        };
        let written = self.runtime.block_on(self.store.put_opts(
            &location,
            PutPayload::from_bytes(payload.into()),
            options,
        ));
        match written {
            Ok(_) => Ok(ClaimOutcome::Held { epoch }),
            // Lost the race: whoever won holds the point; say who.
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => match self.record()? {
                Some((record, _)) => Ok(ClaimOutcome::HeldByOther {
                    holder: record.holder,
                    epoch: record.epoch,
                }),
                None => Err(HostedError(
                    "the ownership write lost a race to a record that then vanished".to_owned(),
                )),
            },
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
