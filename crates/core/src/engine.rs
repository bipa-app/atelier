use std::collections::{BTreeMap, BTreeSet};

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
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefName, WorkspaceNameBuf};
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPath;
use jj_lib::rewrite::rebase_commit;
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{LockedWorkspace, Workspace as JjWorkspace};
use pollster::block_on;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Actor;
use crate::error::{Error, config_err, engine_err};
use crate::workspace::SKIP_NAMES;

const NEW_FILE_SIZE_MAX: u64 = 50 * 1024 * 1024;

/// The largest file the ladder loads to raise its fidelity. Bigger files
/// stay at the binary rung — their deltas are still listed, just not
/// projected or line-diffed — and the caller journals the degradation.
pub(crate) const LADDER_FILE_SIZE_MAX: u64 = 8 * 1024 * 1024;
// The ladder only ever re-reads files a snapshot accepted.
const _: () = assert!(LADDER_FILE_SIZE_MAX <= NEW_FILE_SIZE_MAX);

/// One immutable whole-workspace state in history, attributed to an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot's stable identity.
    pub id: String,
    /// The actor the snapshot is attributed to.
    pub actor: String,
    /// When the snapshot was taken, in unix milliseconds.
    pub at_ms: i64,
    /// The ids of the snapshot's parents in history.
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
    /// The file exceeds [`LADDER_FILE_SIZE_MAX`]; its delta stays at the
    /// binary rung and the caller journals the degradation.
    TooLarge,
    Blob(FileBlob),
}

/// What one undo attempt did to the shared line (ADR-0011).
pub(crate) enum StepBack {
    /// The line stepped back off the landed snapshot to `restored`.
    Stepped { restored: String },
    /// The line already sits on the landed snapshot's parent: a prior
    /// attempt stepped it; nothing to do.
    AlreadyStepped,
    /// The line moved past the landing; `head` is what sits on it now.
    LineMoved { head: String },
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
    jj: JjWorkspace,
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
        let (jj, repo) = block_on(JjWorkspace::init_colocated_git(
            &settings,
            root,
            gix_hash::Kind::Sha1,
        ))
        .map_err(engine_err)?;
        Ok(Self {
            jj,
            repo,
            _settings: settings,
            boundary: boundary.to_vec(),
        })
    }

    /// Load the workspace store already present at `root`.
    pub fn open(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
        let settings = build_settings(actor)?;
        let jj = JjWorkspace::load(
            &settings,
            root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .map_err(engine_err)?;
        let repo = block_on(jj.repo_loader().load_at_head()).map_err(engine_err)?;
        Ok(Self {
            jj,
            repo,
            _settings: settings,
            boundary: boundary.to_vec(),
        })
    }

    /// Reload at the current operation head, folding in operations other
    /// processes (the CLI beside a server) committed since this handle
    /// loaded.
    pub fn refresh(&mut self) -> Result<(), Error> {
        self.repo = block_on(self.jj.repo_loader().load_at_head()).map_err(engine_err)?;
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
        let (mut jj, repo) = JjWorkspace::init_external_git(&settings, root, &root.join(".git"))
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
        let name = jj.workspace_name().to_owned();
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
            jj.check_out(repo.op_id().clone(), None, wc_commit)
                .await
                .map_err(engine_err)?;
        }
        Ok(Self {
            jj,
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
        let name = self.jj.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let options = snapshot_options(base_ignores(&self.boundary)?);

        let mut locked = self
            .jj
            .start_working_copy_mutation()
            .await
            .map_err(engine_err)?;

        let (new_tree, stats) = match locked.locked_wc().snapshot(&options).await {
            Ok(result) => result,
            Err(err) => {
                release_at_old_operation(locked).await?;
                return Err(engine_err(err));
            }
        };
        if !stats.invalid_utf8_paths.is_empty() {
            release_at_old_operation(locked).await?;
            return Err(Error::Engine(
                "working copy has paths with invalid utf-8 names".to_owned(),
            ));
        }

        let wc_commit = self.repo.store().get_commit(&wc_id).map_err(engine_err)?;
        if new_tree.tree_ids() == wc_commit.tree_ids() {
            release_at_old_operation(locked).await?;
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
        let name = self.jj.workspace_name().to_owned();
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
            self.jj.repo_path(),
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
    pub fn land(&mut self, tip: &str, bookmark: &str) -> Result<LandOutcome, Error> {
        block_on(self.land_async(tip, bookmark))
    }

    async fn land_async(&mut self, tip: &str, bookmark: &str) -> Result<LandOutcome, Error> {
        let name = self.jj.workspace_name().to_owned();
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
        // The landed line must be pushable with plain git: move the
        // bookmark to the landed snapshot and export it as a git branch.
        // The working-copy commit itself never enters a branch, so the
        // bookmark - not HEAD - is what a push publishes.
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(bookmark),
            RefTarget::normal(rebased.id().clone()),
        );
        let exported = git::export_refs(tx.repo_mut()).map_err(engine_err)?;
        if !exported.failed_bookmarks.is_empty() {
            return Err(Error::Engine(format!(
                "bookmark {bookmark:?} failed to export: {:?}",
                exported.failed_bookmarks
            )));
        }
        let repo = tx.commit("land").await.map_err(engine_err)?;
        self.repo = repo;
        self.jj
            .check_out(self.repo.op_id().clone(), None, &rebased)
            .await
            .map_err(engine_err)?;
        Ok(LandOutcome::Landed {
            snapshot: rebased.id().hex(),
        })
    }

    /// Step the line back off `landed` to its parent (ADR-0011): the
    /// working copy, the colocated git HEAD, and the bookmark all return;
    /// the landed snapshot stays in history. The caller holds the landing
    /// lease. Idempotent by outcome: a line already stepped says so, a
    /// line that moved past the landing refuses with its head.
    pub fn step_back(&mut self, landed: &str, bookmark: &str) -> Result<StepBack, Error> {
        block_on(self.step_back_async(landed, bookmark))
    }

    async fn step_back_async(&mut self, landed: &str, bookmark: &str) -> Result<StepBack, Error> {
        let name = self.jj.workspace_name().to_owned();
        let head = self.wc_commit_id()?;
        let landed_commit = self.commit_at(landed)?;
        let Some(parent_id) = landed_commit.parent_ids().first() else {
            return Err(Error::Engine(format!(
                "the landed snapshot {landed} has no parent to step back to"
            )));
        };
        if head == *parent_id {
            return Ok(StepBack::AlreadyStepped);
        }
        if head.hex() != landed {
            return Ok(StepBack::LineMoved { head: head.hex() });
        }
        let parent = self
            .repo
            .store()
            .get_commit(parent_id)
            .map_err(engine_err)?;
        let mut tx = self.repo.start_transaction();
        tx.repo_mut()
            .set_wc_commit(name, parent_id.clone())
            .map_err(engine_err)?;
        // The line stepped back; HEAD and the bookmark follow so plain git
        // never publishes the undone head as the newest state.
        git::reset_head(tx.repo_mut(), &parent)
            .await
            .map_err(engine_err)?;
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(bookmark),
            RefTarget::normal(parent_id.clone()),
        );
        let exported = git::export_refs(tx.repo_mut()).map_err(engine_err)?;
        if !exported.failed_bookmarks.is_empty() {
            return Err(Error::Engine(format!(
                "bookmark {bookmark:?} failed to export: {:?}",
                exported.failed_bookmarks
            )));
        }
        let repo = tx.commit("undo").await.map_err(engine_err)?;
        self.repo = repo;
        self.jj
            .check_out(self.repo.op_id().clone(), None, &parent)
            .await
            .map_err(engine_err)?;
        Ok(StepBack::Stepped {
            restored: parent_id.hex(),
        })
    }

    /// Whether `id`'s tree differs from its first parent's — the session
    /// change carries work on this line.
    pub fn tree_changed(&self, id: &str) -> Result<bool, Error> {
        let commit = self.commit_at(id)?;
        let Some(parent_id) = commit.parent_ids().first() else {
            return Ok(true);
        };
        let parent = self
            .repo
            .store()
            .get_commit(parent_id)
            .map_err(engine_err)?;
        Ok(commit.tree_ids() != parent.tree_ids())
    }

    fn wc_commit_id(&self) -> Result<CommitId, Error> {
        let name = self.jj.workspace_name().to_owned();
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
        let name = self.jj.workspace_name().to_owned();
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
            .take(LADDER_FILE_SIZE_MAX + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(engine_err)?;
        if bytes.len() as u64 > LADDER_FILE_SIZE_MAX {
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

    /// Materialize snapshot `id`'s tree at `dest` as a mirror: files are
    /// written (executable bits kept, symlinks recreated), and anything
    /// under `dest` the tree lacks is removed — except the engine-internal
    /// names, which are never touched (ADR-0010).
    pub fn export_tree(&self, id: &str, dest: &Path) -> Result<(), Error> {
        block_on(self.export_tree_async(id, dest))
    }

    async fn export_tree_async(&self, id: &str, dest: &Path) -> Result<(), Error> {
        let empty = self.empty_tree()?;
        let tree = self.tree_at(id)?;
        let mut kept: BTreeSet<String> = BTreeSet::new();
        let mut stream = empty.diff_stream(&tree, &EverythingMatcher);
        while let Some(entry) = stream.next().await {
            let values = entry.values.map_err(engine_err)?;
            let rel = entry.path.as_internal_file_string().to_owned();
            let Some(value) = values.after.as_resolved() else {
                return Err(Error::Engine(format!("conflicted tree entry at {rel}")));
            };
            let Some(value) = value else {
                continue;
            };
            let target = dest.join(&rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            match value {
                TreeValue::File { id, executable, .. } => {
                    let mut reader = self
                        .repo
                        .store()
                        .read_file(&entry.path, id)
                        .await
                        .map_err(engine_err)?;
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).await.map_err(engine_err)?;
                    fs::write(&target, &bytes)?;
                    if *executable {
                        let mut permissions = fs::metadata(&target)?.permissions();
                        permissions.set_mode(0o755);
                        fs::set_permissions(&target, permissions)?;
                    }
                }
                TreeValue::Symlink(id) => {
                    let link = self
                        .repo
                        .store()
                        .read_symlink(&entry.path, id)
                        .await
                        .map_err(engine_err)?;
                    if target.symlink_metadata().is_ok() {
                        fs::remove_file(&target)?;
                    }
                    std::os::unix::fs::symlink(&link, &target)?;
                }
                other => {
                    return Err(Error::Engine(format!("cannot export {rel}: {other:?}")));
                }
            }
            kept.insert(rel);
        }
        drop(stream);
        remove_unkept(dest, &kept)
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
/// Release a locked working copy at the operation it started from: the
/// snapshot found nothing to write, or refused.
async fn release_at_old_operation(mut locked: LockedWorkspace<'_>) -> Result<(), Error> {
    let operation = locked.locked_wc().old_operation_id().clone();
    locked.finish(operation).await.map_err(engine_err)?;
    Ok(())
}

/// How every snapshot walks the working copy: track everything inside the
/// boundary, force nothing, refuse files past the snapshot cap.
fn snapshot_options(base_ignores: Arc<GitIgnoreFile>) -> SnapshotOptions<'static> {
    SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: &EverythingMatcher,
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size: NEW_FILE_SIZE_MAX,
    }
}

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

/// Remove everything under `dir` the exported tree lacks, skipping the
/// engine-internal names at any depth; empty directories vanish with
/// their contents. `root` anchors the tree-relative paths in `kept`.
fn remove_unkept(root: &Path, kept: &BTreeSet<String>) -> Result<(), Error> {
    // An explicit work stack bounds the walk by entry count, never call
    // depth; directories prune deepest-first so an emptied child empties
    // its parent in turn.
    let mut directories: Vec<PathBuf> = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(Error::Engine(format!(
                    "cannot mirror over a non-utf8 name at {}",
                    entry.path().display()
                )));
            };
            if SKIP_NAMES.contains(&name) {
                continue;
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.push(path.clone());
                pending.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .map_err(engine_err)?
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if !kept.contains(&rel) {
                    fs::remove_file(&path)?;
                }
            }
        }
    }
    directories.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in directories {
        if fs::read_dir(&dir)?.next().is_none() {
            fs::remove_dir(&dir)?;
        }
    }
    Ok(())
}
