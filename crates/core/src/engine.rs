use std::collections::BTreeMap;

use atelier_diff_core::{Diff, diff_listings};
use futures::StreamExt;
use jj_lib::backend::CommitId;
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

/// One immutable whole-workspace state in history, attributed to an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub actor: String,
    pub at_ms: i64,
    pub parents: Vec<String>,
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
            None => return Err(Error::Engine("no working-copy commit".to_string())),
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
                "working copy has paths with invalid utf-8 names".to_string(),
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
            None => return Err(Error::Engine("no working-copy commit".to_string())),
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

    /// Diff the latest snapshot against its first parent (empty tree if none).
    pub fn diff_latest(&self) -> Result<Diff, Error> {
        block_on(self.diff_latest_async())
    }

    async fn diff_latest_async(&self) -> Result<Diff, Error> {
        let name = self.ws.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_string())),
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
        tree_diff(&old_tree, &new_tree).await
    }

    /// Diff two snapshots by id: `before` against `after`.
    pub fn diff_between(&self, before: &str, after: &str) -> Result<Diff, Error> {
        block_on(self.diff_between_async(before, after))
    }

    async fn diff_between_async(&self, before: &str, after: &str) -> Result<Diff, Error> {
        let old_tree = self.tree_at(before)?;
        let new_tree = self.tree_at(after)?;
        tree_diff(&old_tree, &new_tree).await
    }

    fn tree_at(&self, id: &str) -> Result<MergedTree, Error> {
        let commit_id = match CommitId::try_from_hex(id) {
            Some(commit_id) => commit_id,
            None => return Err(Error::Engine(format!("not a snapshot id: {id}"))),
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
    let mut config = StackedConfig::with_defaults();
    let text = format!(
        "[user]\nname = \"{name}\"\nemail = \"{email}@atelier.local\"\n",
        name = actor.name,
        email = actor.name,
    );
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

async fn tree_diff(old_tree: &MergedTree, new_tree: &MergedTree) -> Result<Diff, Error> {
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    let mut stream = old_tree.diff_stream(new_tree, &EverythingMatcher);
    while let Some(entry) = stream.next().await {
        let path = entry.path.as_internal_file_string().to_string();
        let values = entry.values.map_err(engine_err)?;
        if values.before.is_present() {
            before.insert(path.clone(), format!("{:?}", values.before));
        }
        if values.after.is_present() {
            after.insert(path, format!("{:?}", values.after));
        }
    }
    Ok(diff_listings(&before, &after))
}
