//! The hosted substrate (ADR-0013): rustyriver embedded behind the
//! house's contained-runtime seam (H1), ownership records and fencing
//! epochs (H2), the node that claims, hydrates, serves, and releases a
//! workspace (H3), and the bucket wiring that makes one URL carry both
//! planes plus the jj/git stores (H4).

use std::fmt;
use std::path::Path;

pub mod area;
pub mod node;
pub mod ownership;
pub mod stores;
pub use area::{ReplicaArea, open_planes};
pub use node::{HostedNode, NodeClaim, NodePaths, ReplicateOutcome};
// Re-exported so hosts (and tests) construct stores against the same
// version the ownership plane speaks.
pub use object_store;
pub use ownership::{ClaimOutcome, Ownership, OwnershipRecord, ReleaseOutcome, file_url};
pub use stores::StoreHeads;

use rustyriver::{Db, FileReplicaClient, ObjectStoreClient, Replica, ReplicaClient, TXID, restore};
use tokio::runtime::Runtime;

/// One failure in the hosted substrate, rendered for the caller's error
/// surface.
#[derive(Debug)]
pub struct HostedError(pub(crate) String);

impl fmt::Display for HostedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostedError {}

pub(crate) fn hosted_err(source: impl fmt::Display) -> HostedError {
    HostedError(source.to_string())
}

/// A workspace store under replication: rustyriver's WAL-to-LTX capture
/// loop bound to one lineage — a file-backed replica for tests and
/// single-machine use, a bucket-backed one for hosted serving. The
/// bucket client is rustyriver's own: it pins `object_store` 0.11 while
/// the record plane speaks 0.14, so the two planes share a bucket
/// through two clients rather than one store handle.
pub struct StoreReplica {
    backend: ReplicaBackend,
    runtime: Runtime,
}

/// The two lineage backends a replica writes to. Boxed: a replica embeds
/// its whole client, and the bucket client dwarfs the file one.
enum ReplicaBackend {
    File(Box<Replica<FileReplicaClient>>),
    Bucket(Box<Replica<ObjectStoreClient>>),
}

impl StoreReplica {
    /// Put the database at `db_path` under replication into `replica_dir`.
    /// rustyriver takes over WAL checkpointing (`wal_autocheckpoint=0`)
    /// and plants its two control tables in the database; the workspace's
    /// own connections keep writing normally.
    pub fn open(db_path: &Path, replica_dir: &Path) -> Result<Self, HostedError> {
        let client = FileReplicaClient::new(replica_dir.display().to_string());
        Self::with_backend(db_path, |db| {
            ReplicaBackend::File(Box::new(Replica::new(db, client)))
        })
    }

    /// Put the database at `db_path` under replication into the bucket
    /// lineage `config` names, through rustyriver's own client.
    pub fn open_bucket(
        db_path: &Path,
        config: rustyriver::ObjectStoreConfig,
    ) -> Result<Self, HostedError> {
        let client = ObjectStoreClient::new(config);
        Self::with_backend(db_path, |db| {
            ReplicaBackend::Bucket(Box::new(Replica::new(db, client)))
        })
    }

    fn with_backend(
        db_path: &Path,
        backend: impl FnOnce(Db) -> ReplicaBackend,
    ) -> Result<Self, HostedError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(hosted_err)?;
        let db = Db::open(db_path).map_err(hosted_err)?;
        Ok(Self {
            backend: backend(db),
            runtime,
        })
    }

    /// Capture everything outstanding and upload it to the replica: the
    /// database's WAL-to-LTX capture first (which establishes its
    /// position), then the replica upload — Litestream's own ordering.
    pub fn sync(&mut self) -> Result<(), HostedError> {
        match &mut self.backend {
            ReplicaBackend::File(replica) => sync_replica(&self.runtime, replica),
            ReplicaBackend::Bucket(replica) => sync_replica(&self.runtime, replica),
        }
    }
}

/// One capture-and-upload pass over any lineage backend.
fn sync_replica<C: ReplicaClient>(
    runtime: &Runtime,
    replica: &mut Replica<C>,
) -> Result<(), HostedError> {
    replica
        .db_mut()
        .ok_or_else(|| HostedError("the replica lost its database".to_owned()))?
        .sync()
        .map_err(hosted_err)?;
    runtime.block_on(replica.sync()).map_err(hosted_err)
}

/// The newest transaction a lineage holds, if any, through any client.
pub(crate) fn latest_txid_with<C: ReplicaClient>(client: &C) -> Result<Option<TXID>, HostedError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(hosted_err)?;
    let files = runtime
        .block_on(client.ltx_files(0, TXID(0), false))
        .map_err(hosted_err)?;
    Ok(files.iter().map(|file| file.max_txid).max())
}

/// Restore a lineage into `output` (which must not exist) as of `txid`,
/// through any client.
pub(crate) fn restore_with<C: ReplicaClient>(
    client: &C,
    output: &Path,
    txid: TXID,
) -> Result<(), HostedError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(hosted_err)?;
    runtime
        .block_on(restore(client, output, txid))
        .map_err(hosted_err)
}

/// The newest transaction the replica at `replica_dir` holds, if any.
pub fn latest_txid(replica_dir: &Path) -> Result<Option<TXID>, HostedError> {
    let client = FileReplicaClient::new(replica_dir.display().to_string());
    latest_txid_with(&client)
}

/// Restore the replica at `replica_dir` into `output` (which must not
/// exist): the newest state, or the state as of `txid` — the point-in-time
/// half of the conformance contract.
pub fn restore_to(
    replica_dir: &Path,
    output: &Path,
    txid: Option<TXID>,
) -> Result<(), HostedError> {
    let client = FileReplicaClient::new(replica_dir.display().to_string());
    let txid = match txid {
        Some(txid) => txid,
        None => match latest_txid(replica_dir)? {
            Some(latest) => latest,
            None => return Err(HostedError("the replica holds no transactions".to_owned())),
        },
    };
    restore_with(&client, output, txid)
}
