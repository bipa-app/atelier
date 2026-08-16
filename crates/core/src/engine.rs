use std::collections::BTreeMap;

use atelier_diff_core::{Diff, diff_listings};
use futures::{AsyncReadExt, StreamExt};
use jj_lib::backend::{CommitId, Signature, Timestamp, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories, default_working_copy_factory,
};
use jj_lib::git::{self, GitImportOptions};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPath;
use jj_lib::rewrite::rebase_commit;
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::Workspace as JjWorkspace;
use pollster::block_on;
use std::fs;
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

/// What one landing attempt did to the shared line.
pub(crate) enum LandOutcome {
    Landed {
        snapshot: String,
    },
    /// The rebase produced conflicts; nothing moved — the shared line
    /// never carries a conflicted state (ADR-0007).
    Conflicted,
}

/// How a snapshot enters history: the shared line stacks a new commit per
/// state; a session amends its one change so the change id survives.
enum SnapshotStyle {
    Stack,
    Amend,
}

/// The jj-backed engine: the only place jj types are allowed to appear.
pub(crate) struct Engine {
    ws: JjWorkspace,
    repo: Arc<ReadonlyRepo>,
    _settings: UserSettings,
    /// Mount names outside this engine's world: never snapshotted as its
    /// content, however they appear (ADR-0009).
    boundary: Vec<String>,
}

impl Engine {
    /// Create a colocated-git workspace store rooted at `root`; paths under
    /// the `boundary` names are outside this engine's world.
    pub fn init(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
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
            boundary: boundary.to_vec(),
        })
    }

    /// Load the workspace store already present at `root`.
    pub fn open(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
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
            boundary: boundary.to_vec(),
        })
    }

    /// Reload at the current operation head, folding in operations other
    /// processes (the CLI beside a server) committed since this handle
    /// loaded.
    pub fn refresh(&mut self) -> Result<(), Error> {
        self.repo = block_on(self.ws.repo_loader().load_at_head()).map_err(engine_err)?;
        Ok(())
    }

    /// Adopt the git repository already at `root`: jj on the existing git
    /// store, its history preserved, HEAD's tree checked out as the
    /// working copy's parent — the repo stays a real repo plain git
    /// pushes (ADR-0009: adopt, never import).
    pub fn adopt_git(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
        block_on(Self::adopt_git_async(root, actor, boundary))
    }

    async fn adopt_git_async(
        root: &Path,
        actor: &Actor,
        boundary: &[String],
    ) -> Result<Self, Error> {
        let settings = build_settings(actor)?;
        let (mut ws, repo) = JjWorkspace::init_external_git(&settings, root, &root.join(".git"))
            .await
            .map_err(engine_err)?;
        let mut tx = repo.start_transaction();
        git::import_head(tx.repo_mut()).await.map_err(engine_err)?;
        let options = GitImportOptions {
            abandon_unreachable_commits: false,
            record_synthetic_predecessors: false,
            remote_auto_track_bookmarks: std::collections::HashMap::new(),
        };
        git::import_refs(tx.repo_mut(), &options)
            .await
            .map_err(engine_err)?;
        let head = tx.repo_mut().view().git_head().as_normal().cloned();
        let name = ws.workspace_name().to_owned();
        let wc_commit = match head {
            // The working copy continues the adopted history: HEAD's tree,
            // HEAD as parent.
            Some(head_id) => {
                let head = tx
                    .repo_mut()
                    .store()
                    .get_commit(&head_id)
                    .map_err(engine_err)?;
                let wc_commit = tx
                    .repo_mut()
                    .new_commit(vec![head_id], head.tree())
                    .set_author(signature(actor))
                    .write()
                    .await
                    .map_err(engine_err)?;
                tx.repo_mut()
                    .set_wc_commit(name, wc_commit.id().clone())
                    .map_err(engine_err)?;
                tx.repo_mut()
                    .rebase_descendants()
                    .await
                    .map_err(engine_err)?;
                Some(wc_commit)
            }
            // An empty repo (no commits yet) adopts as a fresh line.
            None => None,
        };
        if let Some(wc_commit) = &wc_commit {
            git::reset_head(tx.repo_mut(), wc_commit)
                .await
                .map_err(engine_err)?;
        }
        let repo = tx.commit("adopt git repo").await.map_err(engine_err)?;
        if let Some(wc_commit) = &wc_commit {
            ws.check_out(repo.op_id().clone(), None, wc_commit)
                .await
                .map_err(engine_err)?;
        }
        Ok(Self {
            ws,
            repo,
            _settings: settings,
            boundary: boundary.to_vec(),
        })
    }

    /// Snapshot outstanding edits. Records a new commit only when the tree
    /// changed; returns the new snapshot id in that case.
    pub fn snapshot(&mut self) -> Result<Option<String>, Error> {
        block_on(self.snapshot_with(&SnapshotStyle::Stack))
    }

    /// Snapshot outstanding edits by amending this workspace's commit: the
    /// session's change id survives while its tree advances.
    pub fn snapshot_amend(&mut self) -> Result<Option<String>, Error> {
        block_on(self.snapshot_with(&SnapshotStyle::Amend))
    }

    async fn snapshot_with(&mut self, style: &SnapshotStyle) -> Result<Option<String>, Error> {
        let name = self.ws.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let base_ignores = base_ignores(&self.boundary)?;
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
        let new_commit = match style {
            SnapshotStyle::Stack => tx
                .repo_mut()
                .new_commit(vec![wc_id], new_tree)
                .write()
                .await
                .map_err(engine_err)?,
            SnapshotStyle::Amend => tx
                .repo_mut()
                .rewrite_commit(&wc_commit)
                .set_tree(new_tree)
                .write()
                .await
                .map_err(engine_err)?,
        };
        let new_id = new_commit.id().clone();
        tx.repo_mut()
            .set_wc_commit(name, new_id.clone())
            .map_err(engine_err)?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(engine_err)?;
        // The Stack style moves a shared line: keep the colocated git
        // HEAD on it, so plain git sees what jj wrote (PRD story 14). The
        // Amend style is a session's — sessions share the root's git repo
        // and must not steal its HEAD.
        if let SnapshotStyle::Stack = style {
            git::reset_head(tx.repo_mut(), &new_commit)
                .await
                .map_err(engine_err)?;
        }
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

    /// This workspace's working-copy commit: the head of its line.
    pub fn head(&self) -> Result<String, Error> {
        Ok(self.wc_commit_id()?.hex())
    }

    /// The first parent of `id` — for a session's change, the shared-line
    /// snapshot it forked from.
    pub fn parent_of(&self, id: &str) -> Result<String, Error> {
        let commit = self.commit_at(id)?;
        match commit.parent_ids().first() {
            Some(parent) => Ok(parent.hex()),
            None => Err(Error::Engine(format!("snapshot {id} has no parent"))),
        }
    }

    /// Create a session's own jj workspace at `root`: a working copy of the
    /// shared head and a fresh change there, authored by `actor`. The new
    /// change's id.
    pub fn create_session_workspace(
        &mut self,
        root: &Path,
        name: &str,
        actor: &Actor,
    ) -> Result<String, Error> {
        block_on(self.create_session_workspace_async(root, name, actor))
    }

    async fn create_session_workspace_async(
        &mut self,
        root: &Path,
        name: &str,
        actor: &Actor,
    ) -> Result<String, Error> {
        let head_id = self.wc_commit_id()?;
        let head = self.repo.store().get_commit(&head_id).map_err(engine_err)?;
        fs::create_dir_all(root)?;
        let (mut session_ws, repo) = JjWorkspace::init_workspace_with_existing_repo(
            root,
            self.ws.repo_path(),
            &self.repo,
            &*default_working_copy_factory(),
            WorkspaceNameBuf::from(name),
        )
        .await
        .map_err(engine_err)?;
        let mut tx = repo.start_transaction();
        let wc_commit = tx
            .repo_mut()
            .new_commit(vec![head_id], head.tree())
            .set_author(signature(actor))
            .write()
            .await
            .map_err(engine_err)?;
        tx.repo_mut()
            .edit(WorkspaceNameBuf::from(name), &wc_commit)
            .await
            .map_err(engine_err)?;
        // `edit` abandons the placeholder commit the workspace registration
        // created; the abandonment is a rewrite the transaction insists is
        // propagated.
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(engine_err)?;
        let repo = tx.commit("open session").await.map_err(engine_err)?;
        session_ws
            .check_out(repo.op_id().clone(), None, &wc_commit)
            .await
            .map_err(engine_err)?;
        self.repo = repo;
        Ok(wc_commit.change_id().hex())
    }

    /// Land `tip` onto the shared line: rebase it onto the head, refuse a
    /// conflicted result, else advance the line and the working copy. The
    /// caller holds the landing lease.
    pub fn land(&mut self, tip: &str) -> Result<LandOutcome, Error> {
        block_on(self.land_async(tip))
    }

    async fn land_async(&mut self, tip: &str) -> Result<LandOutcome, Error> {
        let name = self.ws.workspace_name().to_owned();
        let head_id = self.wc_commit_id()?;
        let tip_commit = self.commit_at(tip)?;
        let mut tx = self.repo.start_transaction();
        let rebased = rebase_commit(tx.repo_mut(), tip_commit, vec![head_id])
            .await
            .map_err(engine_err)?;
        if rebased.has_conflict() {
            return Ok(LandOutcome::Conflicted);
        }
        tx.repo_mut()
            .set_wc_commit(name, rebased.id().clone())
            .map_err(engine_err)?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(engine_err)?;
        // The line advanced; keep the colocated git HEAD on it.
        git::reset_head(tx.repo_mut(), &rebased)
            .await
            .map_err(engine_err)?;
        let repo = tx.commit("land").await.map_err(engine_err)?;
        self.repo = repo;
        self.ws
            .check_out(self.repo.op_id().clone(), None, &rebased)
            .await
            .map_err(engine_err)?;
        Ok(LandOutcome::Landed {
            snapshot: rebased.id().hex(),
        })
    }

    fn wc_commit_id(&self) -> Result<CommitId, Error> {
        let name = self.ws.workspace_name().to_owned();
        match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => Ok(id.clone()),
            None => Err(Error::Engine("no working-copy commit".to_owned())),
        }
    }

    fn commit_at(&self, id: &str) -> Result<jj_lib::commit::Commit, Error> {
        let Some(commit_id) = CommitId::try_from_hex(id) else {
            return Err(Error::Engine(format!("not a snapshot id: {id}")));
        };
        self.repo.store().get_commit(&commit_id).map_err(engine_err)
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
        Ok(self.commit_at(id)?.tree())
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

/// The commit signature attributing a session's change to its actor; the
/// synthetic address keeps the git backend satisfied, as in
/// [`build_settings`].
fn signature(actor: &Actor) -> Signature {
    Signature {
        name: actor.name.clone(),
        email: format!("{}@atelier.local", actor.name),
        timestamp: Timestamp::now(),
    }
}

/// The engine's boundary as one virtual root .gitignore: its own internals
/// plus the mount names it must never version (anchored, so a nested
/// directory that merely shares a mount's name stays content).
fn base_ignores(boundary: &[String]) -> Result<Arc<GitIgnoreFile>, Error> {
    let mut rules = String::from(".atelier/\n.git/\n.jj/\n");
    for name in boundary {
        rules.push('/');
        rules.push_str(name);
        rules.push_str("/\n");
    }
    GitIgnoreFile::empty()
        .chain(RepoPath::root(), Path::new(".gitignore"), rules.as_bytes())
        .map_err(engine_err)
}
