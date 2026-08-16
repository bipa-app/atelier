//! The hosted substrate's first slice (ADR-0013, H1): rustyriver embedded
//! behind the house's contained-runtime seam. This crate currently proves
//! the wheel — streaming replication and restore of a workspace's store —
//! before ownership records (H2) and serving (H3) lean on it.

use std::fmt;
use std::path::Path;

pub mod ownership;
// Re-exported so hosts (and tests) construct stores against the same
// version the ownership plane speaks.
pub use object_store;
pub use ownership::{ClaimOutcome, Ownership, OwnershipRecord, file_url};

use rustyriver::{Db, FileReplicaClient, Replica, ReplicaClient, TXID, restore};
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
/// loop bound to a file-backed replica. Object-store backends arrive with
/// the ownership slice (H2); the capture semantics are identical.
pub struct StoreReplica {
    replica: Replica<FileReplicaClient>,
    runtime: Runtime,
}

impl StoreReplica {
    /// Put the database at `db_path` under replication into `replica_dir`.
    /// rustyriver takes over WAL checkpointing (`wal_autocheckpoint=0`)
    /// and plants its two control tables in the database; the workspace's
    /// own connections keep writing normally.
    pub fn open(db_path: &Path, replica_dir: &Path) -> Result<Self, HostedError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(hosted_err)?;
        let db = Db::open(db_path).map_err(hosted_err)?;
        let client = FileReplicaClient::new(replica_dir.display().to_string());
        Ok(Self {
            replica: Replica::new(db, client),
            runtime,
        })
    }

    /// Capture everything outstanding and upload it to the replica: the
    /// database's WAL-to-LTX capture first (which establishes its
    /// position), then the replica upload — Litestream's own ordering.
    pub fn sync(&mut self) -> Result<(), HostedError> {
        self.replica
            .db_mut()
            .ok_or_else(|| HostedError("the replica lost its database".to_owned()))?
            .sync()
            .map_err(hosted_err)?;
        self.runtime
            .block_on(self.replica.sync())
            .map_err(hosted_err)
    }
}

/// The newest transaction the replica at `replica_dir` holds, if any.
pub fn latest_txid(replica_dir: &Path) -> Result<Option<TXID>, HostedError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(hosted_err)?;
    let client = FileReplicaClient::new(replica_dir.display().to_string());
    let files = runtime
        .block_on(client.ltx_files(0, TXID(0), false))
        .map_err(hosted_err)?;
    Ok(files.iter().map(|file| file.max_txid).max())
}

/// Restore the replica at `replica_dir` into `output` (which must not
/// exist): the newest state, or the state as of `txid` — the point-in-time
/// half of the conformance contract.
pub fn restore_to(
    replica_dir: &Path,
    output: &Path,
    txid: Option<TXID>,
) -> Result<(), HostedError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(hosted_err)?;
    let client = FileReplicaClient::new(replica_dir.display().to_string());
    let txid = match txid {
        Some(txid) => txid,
        None => match latest_txid(replica_dir)? {
            Some(latest) => latest,
            None => return Err(HostedError("the replica holds no transactions".to_owned())),
        },
    };
    runtime
        .block_on(restore(&client, output, txid))
        .map_err(hosted_err)
}
