use std::collections::BTreeMap;

use atelier_diff_core::{Diff, diff_listings};
use futures::{AsyncReadExt, StreamExt};
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::Workspace as JjWorkspace;
use pollster::block_on;
use std::path::Path;
use std::sync::Arc;

use crate::config::Actor;
use crate::error::{Error, config_err, engine_err};

const MAX_NEW_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// The largest file the ladder loads to raise its fidelity. Bigger files
/// stay at the binary rung — their deltas are still listed, just not
/// projected or line-diffed — and the caller journals the degradation.
pub(crate) const MAX_LADDER_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// One immutable whole-workspace state in history, attributed to an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub actor: String,
    pub at_ms: i64,
    pub parents: Vec<String>,
}

/// The two trees a diff spans, kept opaque so jj types stay inside the
/// engine. The ladder hands it back to read file content off either side.
pub(crate) struct DiffSides {
    before: MergedTree,
    after: MergedTree,
}

/// A file's content on one side of a diff: its content id and its bytes.
pub(crate) struct FileBlob {
    pub id: String,
    pub bytes: Vec<u8>,
}

/// What one side of a diff holds at a path.
pub(crate) enum Side {
    /// The path is absent or not a plain file on this side.
    Absent,
    /// The file exceeds [`MAX_LADDER_FILE_SIZE`]; its delta stays at the
    /// binary rung and the caller journals the degradation.
    TooLarge,
    Blob(FileBlob),
}

/// The jj-backed engine: the only place jj types are allowed to appear.
pub(crate) struct Engine {
    ws: JjWorkspace,
    repo: Arc<ReadonlyRepo>,
    _settings: UserSettings,
}

impl Engine {
    /// Create a colocated-git workspace store rooted at `root`.
    pub fn init(root: &Path, actor: &Actor) -> Result<Self, Error> {
        let settings = build_settings(actor)?;
        let (ws, repo) = block_on(JjWorkspace::init_colocated_git(
            &settings,
            root,
            gix_hash::Kind::Sha1,
        ))
        .map_err(engine_err)?;
        Ok(Self {
            ws,
            repo,
            _settings: settings,
        })
    }

    /// Load the workspace store already present at `root`.
    pub fn open(root: &Path, actor: &Actor) -> Result<Self, Error> {
        let settings = build_settings(actor)?;
        let ws = JjWorkspace::load(
            &settings,
            root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .map_err(engine_err)?;
        let repo = block_on(ws.repo_loader().load_at_head()).map_err(engine_err)?;
        Ok(Self {
            ws,
            repo,
            _settings: settings,
        })
    }

    /// Snapshot outstanding edits. Records a new commit only when the tree
    /// changed; returns the new snapshot id in that case.
    pub fn snapshot(&mut self) -> Result<Option<String>, Error> {
        block_on(self.snapshot_async())
    }

    async fn snapshot_async(&mut self) -> Result<Option<String>, Error> {
        let name = self.ws.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let base_ignores = base_ignores()?;
        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: MAX_NEW_FILE_SIZE,
        };

        let mut locked = self
            .ws
            .start_working_copy_mutation()
            .await
            .map_err(engine_err)?;

        let (new_tree, stats) = match locked.locked_wc().snapshot(&options).await {
            Ok(result) => result,
            Err(err) => {
                let op = locked.locked_wc().old_operation_id().clone();
                locked.finish(op).await.map_err(engine_err)?;
                return Err(engine_err(err));
            }
        };
        if !stats.invalid_utf8_paths.is_empty() {
            let op = locked.locked_wc().old_operation_id().clone();
            locked.finish(op).await.map_err(engine_err)?;
            return Err(Error::Engine(
                "working copy has paths with invalid utf-8 names".to_owned(),
            ));
        }

        let wc_commit = self.repo.store().get_commit(&wc_id).map_err(engine_err)?;
        if new_tree.tree_ids() == wc_commit.tree_ids() {
            let op = locked.locked_wc().old_operation_id().clone();
            locked.finish(op).await.map_err(engine_err)?;
            return Ok(None);
        }

        let mut tx = self.repo.start_transaction();
        tx.set_is_snapshot(true);
        let new_commit = tx
            .repo_mut()
            .new_commit(vec![wc_id], new_tree)
            .write()
            .await
            .map_err(engine_err)?;
        let new_id = new_commit.id().clone();
        tx.repo_mut()
            .set_wc_commit(name, new_id.clone())
            .map_err(engine_err)?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(engine_err)?;
        let repo = tx.commit("snapshot").await.map_err(engine_err)?;
        locked
            .finish(repo.op_id().clone())
            .await
            .map_err(engine_err)?;
        self.repo = repo;
        Ok(Some(new_id.hex()))
    }

    /// The ancestor chain of the working-copy commit, newest first.
    pub fn log(&self, limit: usize) -> Result<Vec<Snapshot>, Error> {
        let name = self.ws.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let root = self.repo.store().root_commit_id().clone();
        let mut out = Vec::new();
        let mut current = Some(wc_id);
        while let Some(id) = current {
            if out.len() >= limit {
                break;
            }
            let commit = self.repo.store().get_commit(&id).map_err(engine_err)?;
            let parents: Vec<String> = commit
                .parent_ids()
                .iter()
                .filter(|parent| **parent != root)
                .map(ObjectId::hex)
                .collect();
            let author = commit.author();
            out.push(Snapshot {
                id: id.hex(),
                actor: author.name.clone(),
                at_ms: author.timestamp.timestamp.0,
                parents,
            });
            current = commit
                .parent_ids()
                .iter()
                .find(|parent| **parent != root)
                .cloned();
        }
        Ok(out)
    }

    /// Diff the latest snapshot against its first parent (empty tree if
    /// none), returning the binary-rung diff plus the sides it spans.
    pub fn diff_latest(&self) -> Result<(Diff, DiffSides), Error> {
        block_on(self.diff_latest_async())
    }

    async fn diff_latest_async(&self) -> Result<(Diff, DiffSides), Error> {
        let name = self.ws.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let root = self.repo.store().root_commit_id().clone();
        let commit = self.repo.store().get_commit(&wc_id).map_err(engine_err)?;
        let new_tree = commit.tree();
        let parent = commit
            .parent_ids()
            .iter()
            .find(|parent| **parent != root)
            .cloned();
        let old_tree = match parent {
            Some(parent_id) => self
                .repo
                .store()
                .get_commit(&parent_id)
                .map_err(engine_err)?
                .tree(),
            None => self.empty_tree()?,
        };
        self.tree_diff(old_tree, new_tree).await
    }

    /// Diff two snapshots by id: `before` against `after`, returning the
    /// binary-rung diff plus the sides it spans.
    pub fn diff_between(&self, before: &str, after: &str) -> Result<(Diff, DiffSides), Error> {
        block_on(self.diff_between_async(before, after))
    }

    async fn diff_between_async(
        &self,
        before: &str,
        after: &str,
    ) -> Result<(Diff, DiffSides), Error> {
        let old_tree = self.tree_at(before)?;
        let new_tree = self.tree_at(after)?;
        self.tree_diff(old_tree, new_tree).await
    }

    /// The file at `path` on each side of the diff.
    pub fn read_file_sides(&self, sides: &DiffSides, path: &str) -> Result<(Side, Side), Error> {
        block_on(async {
            let path = RepoPath::from_internal_string(path).map_err(engine_err)?;
            let before = self.file_blob(&sides.before, path).await?;
            let after = self.file_blob(&sides.after, path).await?;
            Ok((before, after))
        })
    }

    async fn file_blob(&self, tree: &MergedTree, path: &RepoPath) -> Result<Side, Error> {
        let value = tree.path_value(path).await.map_err(engine_err)?;
        let Some(Some(TreeValue::File { id, .. })) = value.as_resolved() else {
            return Ok(Side::Absent);
        };
        let reader = self
            .repo
            .store()
            .read_file(path, id)
            .await
            .map_err(engine_err)?;
        let mut bytes = Vec::new();
        reader
            .take(MAX_LADDER_FILE_SIZE + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(engine_err)?;
        if bytes.len() as u64 > MAX_LADDER_FILE_SIZE {
            return Ok(Side::TooLarge);
        }
        Ok(Side::Blob(FileBlob {
            id: id.hex(),
            bytes,
        }))
    }

    async fn tree_diff(
        &self,
        old_tree: MergedTree,
        new_tree: MergedTree,
    ) -> Result<(Diff, DiffSides), Error> {
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        let mut stream = old_tree.diff_stream(&new_tree, &EverythingMatcher);
        while let Some(entry) = stream.next().await {
            let path = entry.path.as_internal_file_string().to_owned();
            let values = entry.values.map_err(engine_err)?;
            if values.before.is_present() {
                before.insert(path.clone(), format!("{:?}", values.before));
            }
            if values.after.is_present() {
                after.insert(path, format!("{:?}", values.after));
            }
        }
        drop(stream);
        let diff = diff_listings(&before, &after);
        Ok((
            diff,
            DiffSides {
                before: old_tree,
                after: new_tree,
            },
        ))
    }

    fn tree_at(&self, id: &str) -> Result<MergedTree, Error> {
        let Some(commit_id) = CommitId::try_from_hex(id) else {
            return Err(Error::Engine(format!("not a snapshot id: {id}")));
        };
        let commit = self
            .repo
            .store()
            .get_commit(&commit_id)
            .map_err(engine_err)?;
        Ok(commit.tree())
    }

    fn empty_tree(&self) -> Result<MergedTree, Error> {
        let root = self.repo.store().root_commit_id().clone();
        let commit = self.repo.store().get_commit(&root).map_err(engine_err)?;
        Ok(commit.tree())
    }
}

fn build_settings(actor: &Actor) -> Result<UserSettings, Error> {
    #[derive(serde::Serialize)]
    struct UserConfig<'a> {
        user: UserSection<'a>,
    }

    #[derive(serde::Serialize)]
    struct UserSection<'a> {
        name: &'a str,
        email: String,
    }

    let mut config = StackedConfig::with_defaults();
    let text = toml::to_string(&UserConfig {
        user: UserSection {
            name: &actor.name,
            email: format!("{}@atelier.local", actor.name),
        },
    })
    .map_err(config_err)?;
    let layer = ConfigLayer::parse(ConfigSource::User, &text).map_err(config_err)?;
    config.add_layer(layer);
    UserSettings::from_config(config).map_err(config_err)
}

fn base_ignores() -> Result<Arc<GitIgnoreFile>, Error> {
    GitIgnoreFile::empty()
        .chain(
            RepoPath::root(),
            Path::new(".gitignore"),
            b".atelier/\n.git/\n.jj/\n",
        )
        .map_err(engine_err)
}
