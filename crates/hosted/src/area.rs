//! The replica area (ADR-0013, H4): where a workspace's `SQLite` lineages
//! live, one `e<epoch>` lineage per activation. File-backed for tests and
//! single-machine use, bucket-backed for `atelier serve --hosted` — the
//! same `ltx/e<epoch>/` keys the ownership plane writes, so one bucket
//! carries both planes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use rustyriver::replica_url::{is_local_endpoint, parse_replica_url_with_query};
use rustyriver::{FileReplicaClient, ObjectStoreClient, ObjectStoreConfig};

use crate::ownership::Ownership;
use crate::{HostedError, StoreReplica, hosted_err, latest_txid_with, restore_with};
use rustyriver::TXID;

/// Where a workspace's lineages replicate into.
#[derive(Clone)]
pub enum ReplicaArea {
    /// A directory of `e<epoch>` lineage subdirectories — the file-side
    /// mirror of the bucket's `ltx/e<epoch>/` keys.
    Files(PathBuf),
    /// The workspace prefix in an S3-compatible bucket, shared with the
    /// ownership plane: lineages live at `<prefix>/ltx/e<epoch>/`.
    Bucket(Box<ObjectStoreConfig>),
}

impl ReplicaArea {
    /// The newest transaction `epoch`'s lineage holds, if any.
    pub fn latest_txid(&self, epoch: u64) -> Result<Option<TXID>, HostedError> {
        match self {
            Self::Files(root) => {
                let client = FileReplicaClient::new(lineage_dir(root, epoch));
                latest_txid_with(&client)
            }
            Self::Bucket(config) => latest_txid_with(&bucket_client(config, epoch)),
        }
    }

    /// Restore `epoch`'s lineage into `output` (which must not exist),
    /// point-in-time at `txid` — the transaction its manifest pinned.
    pub fn restore(&self, epoch: u64, output: &Path, txid: TXID) -> Result<(), HostedError> {
        match self {
            Self::Files(root) => {
                let client = FileReplicaClient::new(lineage_dir(root, epoch));
                restore_with(&client, output, txid)
            }
            Self::Bucket(config) => restore_with(&bucket_client(config, epoch), output, txid),
        }
    }

    /// Put the database at `db_path` under replication into `epoch`'s
    /// lineage.
    pub fn replicate(&self, db_path: &Path, epoch: u64) -> Result<StoreReplica, HostedError> {
        match self {
            Self::Files(root) => StoreReplica::open(db_path, Path::new(&lineage_dir(root, epoch))),
            Self::Bucket(config) => StoreReplica::open_bucket(db_path, epoch_config(config, epoch)),
        }
    }
}

/// rustyriver's client over one epoch's bucket lineage.
fn bucket_client(config: &ObjectStoreConfig, epoch: u64) -> ObjectStoreClient {
    ObjectStoreClient::new(epoch_config(config, epoch))
}

/// A lineage's directory in a file-backed replica area.
fn lineage_dir(root: &Path, epoch: u64) -> String {
    root.join(format!("e{epoch}")).display().to_string()
}

/// The base config repointed at one epoch's lineage keys.
fn epoch_config(config: &ObjectStoreConfig, epoch: u64) -> ObjectStoreConfig {
    let mut config = config.clone();
    config.path = format!("{}/ltx/e{epoch}", config.path);
    config
}

/// Open both planes of one workspace bucket from one S3-compatible URL
/// (`s3://bucket/prefix`, `MinIO` and R2 via `?endpoint=`; credentials from
/// `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`): the ownership plane on
/// the record store, the replica area on rustyriver's client — two
/// clients, one bucket, the same keys.
pub fn open_planes(url: &str) -> Result<(Ownership, ReplicaArea), HostedError> {
    let parsed = parse_replica_url_with_query(url).map_err(hosted_err)?;
    if parsed.scheme != "s3" {
        return Err(HostedError(format!(
            "hosted workspaces speak s3-compatible bucket URLs; {url:?} does not"
        )));
    }
    let config = ObjectStoreConfig::from_url(&parsed).map_err(hosted_err)?;
    // An absent path parses as Go's cleaned "." — both mean no prefix.
    if config.path.is_empty() || config.path == "." {
        return Err(HostedError(
            "the bucket URL must name the workspace prefix (s3://bucket/<prefix>)".to_owned(),
        ));
    }
    let store = record_store(&config)?;
    let ownership = Ownership::from_store(store, ObjectPath::from(config.path.clone()))?;
    Ok((ownership, ReplicaArea::Bucket(Box::new(config))))
}

/// The record plane's store over the same bucket the replica client
/// writes: the same bucket, region, endpoint, credential, and path-style
/// decisions rustyriver's own builder makes, so both planes agree on what
/// one URL means.
fn record_store(config: &ObjectStoreConfig) -> Result<Arc<dyn ObjectStore>, HostedError> {
    let region = if config.region.is_empty() {
        "us-east-1"
    } else {
        &config.region
    };
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&config.bucket)
        .with_region(region)
        .with_virtual_hosted_style_request(!config.force_path_style);
    if !config.endpoint.is_empty() {
        let allow_http = config.endpoint.starts_with("http://")
            || config.skip_verify
            || is_local_endpoint(&config.endpoint);
        builder = builder
            .with_endpoint(&config.endpoint)
            .with_allow_http(allow_http);
    }
    if !config.access_key_id.is_empty() {
        builder = builder.with_access_key_id(&config.access_key_id);
    }
    if !config.secret_access_key.is_empty() {
        builder = builder.with_secret_access_key(&config.secret_access_key);
    }
    let store = builder.build().map_err(hosted_err)?;
    Ok(Arc::new(store))
}
