//! The hosted node (ADR-0013, H3): serving is claiming. A node claims a
//! workspace's ownership record, hydrates the store from the newest
//! surviving lineage, replicates under the held epoch, and releases on
//! shutdown. Every replication acknowledges by re-reading the record, so
//! a deposed node's replication surfaces refusal — its bytes landed in a
//! superseded lineage and promise nothing.

use std::path::{Path, PathBuf};

use crate::ownership::{ClaimOutcome, Ownership, ReleaseOutcome};
use crate::{HostedError, StoreReplica, latest_txid, restore_to};

/// Where a hosted node keeps a workspace on its own machine: the store it
/// serves and the replica area lineages replicate into. The replica area
/// mirrors the ownership plane's `ltx/e<epoch>/` key layout — one
/// directory per activation. It is file-backed until rustyriver speaks
/// the same `object_store` version as the record plane.
pub struct NodePaths {
    /// The live `SQLite` store this node serves.
    pub store: PathBuf,
    /// The replica area: one `e<epoch>` lineage per activation.
    pub replica_root: PathBuf,
}

/// What one node activation produced.
pub enum NodeClaim {
    /// The record names this node: it is serving. Boxed: a node carries
    /// two contained runtimes and dwarfs the refusal arm.
    Serving(Box<HostedNode>),
    /// Another holder owns the workspace; claiming never seizes — that
    /// is `take_over`, a deliberate act.
    HeldByOther {
        /// The node session that holds the workspace.
        holder: String,
        /// The epoch the holder writes under.
        epoch: u64,
    },
}

/// What one replication pass produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicateOutcome {
    /// The record still names this node: the captured acts are
    /// acknowledged — the surviving lineage carries them.
    Acknowledged,
    /// The record moved on: the captured bytes landed in a superseded
    /// lineage and promise nothing.
    Deposed,
}

/// One node serving one hosted workspace: the ownership record it holds,
/// the epoch it writes under, and the store replication bound to that
/// epoch's lineage.
pub struct HostedNode {
    ownership: Ownership,
    replica: StoreReplica,
    holder: String,
    epoch: u64,
}

impl HostedNode {
    /// Claim the workspace for `holder` and serve it: hydrate the store
    /// from the newest surviving lineage when this node has none, then
    /// replicate under the claimed epoch. A record naming another holder
    /// refuses.
    pub fn claim(
        ownership: Ownership,
        holder: &str,
        paths: &NodePaths,
    ) -> Result<NodeClaim, HostedError> {
        match ownership.claim(holder)? {
            ClaimOutcome::Held { epoch } => Ok(NodeClaim::Serving(Box::new(Self::activate(
                ownership, holder, epoch, paths,
            )?))),
            ClaimOutcome::HeldByOther { holder, epoch } => {
                Ok(NodeClaim::HeldByOther { holder, epoch })
            }
        }
    }

    /// Seize the workspace for `holder` — the previous holder crashed or
    /// is partitioned — and serve it exactly as `claim` would. The epoch
    /// advances, so the deposed writer's lineage is superseded.
    pub fn take_over(
        ownership: Ownership,
        holder: &str,
        paths: &NodePaths,
    ) -> Result<NodeClaim, HostedError> {
        match ownership.take_over(holder)? {
            ClaimOutcome::Held { epoch } => Ok(NodeClaim::Serving(Box::new(Self::activate(
                ownership, holder, epoch, paths,
            )?))),
            ClaimOutcome::HeldByOther { holder, epoch } => {
                Ok(NodeClaim::HeldByOther { holder, epoch })
            }
        }
    }

    /// The epoch this node writes under — the fence in every key.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Capture and upload outstanding acts, then acknowledge: the record
    /// must still name this node at this epoch, else the refusal
    /// surfaces. The upload itself never refuses — the fence is the epoch
    /// in the lineage, not a condition on the write.
    pub fn replicate(&mut self) -> Result<ReplicateOutcome, HostedError> {
        self.replica.sync()?;
        if self.ownership.confirm(&self.holder, self.epoch)? {
            Ok(ReplicateOutcome::Acknowledged)
        } else {
            Ok(ReplicateOutcome::Deposed)
        }
    }

    /// One final replication, then the guarded record write: a released
    /// workspace keeps its epoch as the high-water mark and any node may
    /// claim it. A node deposed before its final capture releases
    /// nothing — the workspace already moved on.
    pub fn release(mut self) -> Result<ReleaseOutcome, HostedError> {
        match self.replicate()? {
            ReplicateOutcome::Acknowledged => self.ownership.release(&self.holder, self.epoch),
            ReplicateOutcome::Deposed => Ok(ReleaseOutcome::NotHeld),
        }
    }

    /// Hydrate and bind: with no local store, restore the newest
    /// surviving lineage; with no lineage, the local store seeds the
    /// bucket. Both at once disagree about the truth and refuse — local
    /// state on a hosted node is derived, never authoritative.
    fn activate(
        ownership: Ownership,
        holder: &str,
        epoch: u64,
        paths: &NodePaths,
    ) -> Result<HostedNode, HostedError> {
        let lineage = newest_lineage(&paths.replica_root, epoch)?;
        match (paths.store.exists(), lineage) {
            (true, None) => {}
            (false, Some(prior)) => {
                restore_to(&lineage_dir(&paths.replica_root, prior), &paths.store, None)?;
            }
            (true, Some(_)) => {
                return Err(HostedError(
                    "the local store would shadow the bucket's lineage; remove it and hydrate"
                        .to_owned(),
                ));
            }
            (false, None) => {
                return Err(HostedError(
                    "the workspace has no store to serve: no local store, no lineage".to_owned(),
                ));
            }
        }
        let own = lineage_dir(&paths.replica_root, epoch);
        if latest_txid(&own)?.is_some() {
            return Err(HostedError(format!(
                "epoch {epoch} already has a lineage; the replica area and the record disagree"
            )));
        }
        let replica = StoreReplica::open(&paths.store, &own)?;
        Ok(HostedNode {
            ownership,
            replica,
            holder: holder.to_owned(),
            epoch,
        })
    }
}

/// The newest epoch below `epoch` whose lineage holds a transaction —
/// where a hydration restores from. An activation that died before its
/// first capture leaves no lineage and is skipped.
fn newest_lineage(replica_root: &Path, epoch: u64) -> Result<Option<u64>, HostedError> {
    for prior in (1..epoch).rev() {
        if latest_txid(&lineage_dir(replica_root, prior))?.is_some() {
            return Ok(Some(prior));
        }
    }
    Ok(None)
}

/// A lineage's directory in the replica area — the file-side mirror of
/// the ownership plane's `ltx/e<epoch>/` key prefix.
fn lineage_dir(replica_root: &Path, epoch: u64) -> PathBuf {
    replica_root.join(format!("e{epoch}"))
}
