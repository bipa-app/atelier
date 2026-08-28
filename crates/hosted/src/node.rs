//! The hosted node (ADR-0013, H3/H4): serving is claiming. A node claims
//! a workspace's ownership record, hydrates the stores from the newest
//! surviving lineage — the `SQLite` store through rustyriver, the jj/git
//! stores through the manifest — replicates under the held epoch, and
//! releases on shutdown. Every replication acknowledges by re-reading the
//! record, so a deposed node's replication surfaces refusal — its bytes
//! landed in a superseded lineage and promise nothing.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use crate::area::ReplicaArea;
use crate::ownership::{ClaimOutcome, Ownership, ReleaseOutcome};
use crate::{HostedError, StoreReplica, stores};

/// Where a hosted node keeps a workspace on its own machine, and where
/// its lineages replicate into.
pub struct NodePaths {
    /// The live `SQLite` store this node serves.
    pub store: PathBuf,
    /// The workspace root whose engine stores (jj and git, root and
    /// mounts) replicate beside the `SQLite` store. A root without
    /// engine stores replicates the store alone.
    pub root: PathBuf,
    /// The replica area lineages replicate into — one `e<epoch>` lineage
    /// per activation, file-backed or in the ownership plane's bucket.
    pub replica: ReplicaArea,
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
    area: ReplicaArea,
    holder: String,
    epoch: u64,
    root: PathBuf,
    /// Content ids already in the bucket: engine-store uploads dedupe
    /// against it, so unchanged files cost one hash, never a round trip.
    objects: BTreeSet<String>,
}

impl HostedNode {
    /// Claim the workspace for `holder` and serve it: hydrate the stores
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
    ///
    /// The `SQLite` store captures first and the manifest pins last —
    /// with the exact transaction it covers — so hydration always
    /// restores both stores from one completed pass.
    pub fn replicate(&mut self) -> Result<ReplicateOutcome, HostedError> {
        self.replica.sync()?;
        let txid = match self.area.latest_txid(self.epoch)? {
            Some(txid) => txid.0,
            None => {
                return Err(HostedError(
                    "the lineage holds no transactions after a sync".to_owned(),
                ));
            }
        };
        stores::capture(
            &self.root,
            &self.ownership,
            self.epoch,
            txid,
            &mut self.objects,
        )?;
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
    /// surviving lineage — `SQLite` and engine stores from that one
    /// lineage; with no lineage, the local store seeds the bucket. Both
    /// at once disagree about the truth and refuse — local state on a
    /// hosted node is derived, never authoritative.
    fn activate(
        ownership: Ownership,
        holder: &str,
        epoch: u64,
        paths: &NodePaths,
    ) -> Result<HostedNode, HostedError> {
        let lineage = stores::newest_heads(&ownership, epoch)?;
        match (paths.store.exists(), lineage) {
            (true, None) => {}
            (false, Some((prior, heads))) => {
                if let Some(parent) = paths.store.parent() {
                    fs::create_dir_all(parent).map_err(crate::hosted_err)?;
                }
                paths
                    .replica
                    .restore(prior, &paths.store, rustyriver::TXID(heads.txid))?;
                stores::hydrate(&paths.root, &ownership, &heads)?;
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
        if paths.replica.latest_txid(epoch)?.is_some() {
            return Err(HostedError(format!(
                "epoch {epoch} already has a lineage; the replica area and the record disagree"
            )));
        }
        let replica = paths.replica.replicate(&paths.store, epoch)?;
        let objects = ownership.objects()?;
        Ok(HostedNode {
            ownership,
            replica,
            area: paths.replica.clone(),
            holder: holder.to_owned(),
            epoch,
            root: paths.root.clone(),
            objects,
        })
    }
}
