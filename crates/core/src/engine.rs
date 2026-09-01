use std::collections::{BTreeMap, BTreeSet};

use atelier_sdk_diff::{Diff, diff_listings};
use futures::{AsyncReadExt, StreamExt};
use jj_lib::backend::{CommitId, Signature, Timestamp, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories, default_working_copy_factory,
};
use jj_lib::file_util;
use jj_lib::git::{self, GitImportOptions};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefName, WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo, RepoLoader};
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponent};
use jj_lib::rewrite::{merge_commit_trees, rebase_commit};
use jj_lib::settings::UserSettings;
use jj_lib::transaction::Transaction;
use jj_lib::working_copy::{SnapshotOptions, WorkingCopy};
use jj_lib::workspace::{LockedWorkspace, Workspace as JjWorkspace};
use pollster::block_on;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Actor, ActorKind, GitIdentity, SigningBackend, resolve_git_identity};
use crate::error::{Error, config_err, engine_err};
use crate::workspace::SKIP_NAMES;

const NEW_FILE_SIZE_MAX: u64 = 50 * 1024 * 1024;

/// The largest file the ladder loads to raise its fidelity. Bigger files
/// stay at the binary rung — their deltas are still listed, just not
/// projected or line-diffed — and the caller journals the degradation.
pub(crate) const LADDER_FILE_SIZE_MAX: u64 = 8 * 1024 * 1024;
// The ladder only ever re-reads files a snapshot accepted.
const _: () = assert!(LADDER_FILE_SIZE_MAX <= NEW_FILE_SIZE_MAX);

/// The largest stretch of moved git history a fold scans for a commit
/// carrying the line's exact tree; past it the fold falls back to a
/// merge. Bounds the walk the way every input is bounded — out-of-band
/// history is user-controlled.
const FOLD_SCAN_MAX: usize = 1000;

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
#[derive(Debug)]
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

/// What one fold attempt did with a colocated git repo that moved out of
/// band (an external commit, branch move, or push).
pub(crate) enum GitFold {
    /// Nothing moved beneath jj's view; the line stands.
    Current,
    /// The moved git state folded into the line; `head` is the line now.
    Folded { head: String },
}

/// The fence a leased line move runs at its last pure moment — after
/// the long phases, before the first externally visible write (a git
/// ref, an operation commit). It renews a standing tenancy and refuses
/// a superseded one; on refusal nothing has published.
pub(crate) type Fence<'a> = &'a dyn Fn() -> Result<(), Error>;

/// How a snapshot enters history: the shared line stacks a new commit
/// per state; the fold's pre-snapshot stacks without moving git HEAD —
/// an out-of-band move may have put HEAD exactly where the fold must
/// read it; a session amends its one change so the change id survives.
enum SnapshotStyle {
    Stack,
    StackKeepHead,
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
    /// The publishing identity, when one is configured (ADR-0015);
    /// per-actor author stamps consult it.
    git: Option<GitIdentity>,
    /// The author this engine stamps on the commits it creates, from its
    /// construction actor: name, then address.
    author: (String, String),
}

impl Engine {
    /// Create a colocated-git workspace store rooted at `root`; paths under
    /// the `boundary` names are outside this engine's world.
    pub fn init(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
        let git = resolve_git_identity()?;
        let settings = build_settings(actor, git.as_ref())?;
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
            author: author_identity(actor, git.as_ref()),
            git,
        })
    }

    /// The signature this engine stamps as author on the commits it
    /// creates; rewrites and rebases keep the original authors.
    fn author(&self) -> Signature {
        stamp(self.author.clone())
    }

    /// Load the workspace store already present at `root`.
    pub fn open(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
        let git = resolve_git_identity()?;
        let settings = build_settings(actor, git.as_ref())?;
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
            author: author_identity(actor, git.as_ref()),
            git,
        })
    }

    /// Load the workspace store at `root` whose working copy a hydration
    /// left absent, and rebuild it: the recorded working-copy commit
    /// checks out into a fresh working copy — derived state
    /// rematerializes from history (ADR-0013).
    pub fn rematerialize(root: &Path, actor: &Actor, boundary: &[String]) -> Result<Self, Error> {
        block_on(Self::rematerialize_async(root, actor, boundary))
    }

    async fn rematerialize_async(
        root: &Path,
        actor: &Actor,
        boundary: &[String],
    ) -> Result<Self, Error> {
        let git = resolve_git_identity()?;
        let settings = build_settings(actor, git.as_ref())?;
        // Canonical, as the workspace loader would keep it: session
        // registrations compute pointers relative to this path.
        let repo_path = fs::canonicalize(root.join(".jj").join("repo"))?;
        let loader =
            RepoLoader::init_from_file_system(&settings, &repo_path, &default_backend_factories())
                .map_err(engine_err)?;
        let repo = loader.load_at_head().await.map_err(engine_err)?;
        let name = WorkspaceName::DEFAULT;
        let wc_id = match repo.view().get_wc_commit_id(name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let wc_commit = repo.store().get_commit(&wc_id).map_err(engine_err)?;
        let working_copy =
            init_absent_working_copy(&repo, root, &root.join(".jj"), name.to_owned())?;
        let mut jj = JjWorkspace::new(root, repo_path, working_copy, loader).map_err(engine_err)?;
        jj.check_out(repo.op_id().clone(), None, &wc_commit)
            .await
            .map_err(engine_err)?;
        Ok(Self {
            jj,
            repo,
            _settings: settings,
            boundary: boundary.to_vec(),
            author: author_identity(actor, git.as_ref()),
            git,
        })
    }

    /// Rebuild one session's working copy at `root`: history records the
    /// session workspace and its commit; only the on-disk state is absent
    /// after a hydration. The engine's own store stays untouched — this
    /// is jj's own registration minus the new commit it would create.
    pub fn rematerialize_session_workspace(&self, root: &Path, name: &str) -> Result<(), Error> {
        block_on(self.rematerialize_session_async(root, name))
    }

    async fn rematerialize_session_async(&self, root: &Path, name: &str) -> Result<(), Error> {
        let ws_name = WorkspaceNameBuf::from(name);
        let wc_id = match self.repo.view().get_wc_commit_id(&ws_name) {
            Some(id) => id.clone(),
            None => {
                return Err(Error::Engine(format!(
                    "history records no workspace {name}"
                )));
            }
        };
        let wc_commit = self.repo.store().get_commit(&wc_id).map_err(engine_err)?;
        fs::create_dir_all(root)?;
        let jj_dir = root.join(".jj");
        fs::create_dir(&jj_dir)?;
        // The session's store is the primary repo; the pointer is kept
        // relative, so the workspace moves whole.
        let repo_dir = self.jj.repo_path().to_path_buf();
        let jj_dir_abs = fs::canonicalize(&jj_dir)?;
        let pointer = file_util::relative_path(&jj_dir_abs, &repo_dir);
        let pointer = if pointer.is_relative() {
            file_util::slash_path(&pointer).into_owned()
        } else {
            pointer
        };
        let bytes = file_util::path_to_bytes(&pointer).map_err(engine_err)?;
        fs::write(jj_dir.join("repo"), bytes)?;
        let working_copy = init_absent_working_copy(&self.repo, root, &jj_dir, ws_name)?;
        let mut session_ws =
            JjWorkspace::new(root, repo_dir, working_copy, self.jj.repo_loader().clone())
                .map_err(engine_err)?;
        session_ws
            .check_out(self.repo.op_id().clone(), None, &wc_commit)
            .await
            .map_err(engine_err)?;
        Ok(())
    }

    /// Reload at the current operation head, folding in operations other
    /// processes (the CLI beside a server) committed since this handle
    /// loaded.
    pub fn refresh(&mut self) -> Result<(), Error> {
        self.repo = block_on(self.jj.repo_loader().load_at_head()).map_err(engine_err)?;
        Ok(())
    }

    /// Whether the colocated git repo moved beneath jj's view — an
    /// out-of-band commit, branch move, or push. The probe imports into
    /// a transaction it drops; nothing changes.
    pub fn git_moved(&self) -> Result<bool, Error> {
        block_on(self.git_moved_async())
    }

    async fn git_moved_async(&self) -> Result<bool, Error> {
        let mut tx = self.repo.start_transaction();
        git::import_head(tx.repo_mut()).await.map_err(engine_err)?;
        git::import_refs(tx.repo_mut(), &import_options())
            .await
            .map_err(engine_err)?;
        Ok(tx.repo_mut().has_changes())
    }

    /// Fold an out-of-band git move into the line: import the moved
    /// HEAD and refs, then put the working copy on the moved target —
    /// `branch`'s when it names one, git HEAD's otherwise. A target
    /// built on the line, carrying its exact tree, or carrying it
    /// somewhere in the moved history (a rewrite with follow-up work)
    /// becomes the line directly; other divergence merges through the
    /// common ancestor into one fold state. A content conflict refuses
    /// by name: the shared line never carries a conflicted state
    /// (ADR-0007). Open sessions keep their fork points and merge at
    /// landing, exactly as they do when another session lands first.
    /// The caller holds the line's landing lease and snapshots first;
    /// `fresh_snapshot` says that snapshot recorded new work, which the
    /// history scan must never mistake for rewritten history — fresh
    /// work merges, it is never superseded.
    pub fn fold_git(
        &mut self,
        branch: &str,
        fresh_snapshot: bool,
        fence: Fence<'_>,
    ) -> Result<GitFold, Error> {
        block_on(self.fold_git_async(branch, fresh_snapshot, fence))
    }

    async fn fold_git_async(
        &mut self,
        branch: &str,
        fresh_snapshot: bool,
        fence: Fence<'_>,
    ) -> Result<GitFold, Error> {
        let name = self.jj.workspace_name().to_owned();
        let mut tx = self.repo.start_transaction();
        git::import_head(tx.repo_mut()).await.map_err(engine_err)?;
        git::import_refs(tx.repo_mut(), &import_options())
            .await
            .map_err(engine_err)?;
        if !tx.repo_mut().has_changes() {
            return Ok(GitFold::Current);
        }
        let target_id = line_target(&mut tx, branch);
        let wc_id = self.wc_commit_id()?;
        // Refs moved but the line's target did not leave it: absorb the
        // imports so the next export speaks from git's current state.
        let Some(target_id) = target_id else {
            return self.absorb_refs(tx, fence).await;
        };
        if target_id == wc_id {
            return self.absorb_refs(tx, fence).await;
        }
        let wc_commit = self.repo.store().get_commit(&wc_id).map_err(engine_err)?;
        let target = self
            .repo
            .store()
            .get_commit(&target_id)
            .map_err(engine_err)?;
        if wc_commit.parent_ids() == [target_id.clone()] {
            return self.absorb_refs(tx, fence).await;
        }
        let line = if is_ancestor(&tx, &wc_id, &target_id).await? {
            // The move built on the line (a plain push of follow-up
            // work): fast-forward, never replay the line's own diff.
            target
        } else if wc_commit.tree_ids() == target.tree_ids() {
            // The moved tip carries the line's exact content (a
            // recommit, a message rewrite, a conceded conflict): the
            // moved commit IS the line now.
            target
        } else if !fresh_snapshot && self.line_content_in(&tx, &wc_commit, &target_id).await? {
            // The moved history carries the line's exact content with
            // follow-up work on top: the moved tip supersedes the line
            // whole. Never taken for freshly snapshotted work — an old
            // tree match must not discard what the disk just said.
            target
        } else {
            let merged = merge_commit_trees(tx.repo(), &[wc_commit, target])
                .await
                .map_err(engine_err)?;
            if merged.has_conflict() {
                // The refusal is consumed as this line's state — a
                // superseded holder must not report a conflict its
                // rival may already have resolved.
                fence()?;
                return Err(Error::GitFoldConflicted {
                    branch: branch.to_owned(),
                });
            }
            tx.repo_mut()
                .new_commit(vec![target_id], merged)
                .set_author(self.author())
                .set_description("fold")
                .write()
                .await
                .map_err(engine_err)?
        };
        fence()?;
        tx.repo_mut()
            .set_wc_commit(name, line.id().clone())
            .map_err(engine_err)?;
        git::reset_head(tx.repo_mut(), &line)
            .await
            .map_err(engine_err)?;
        let repo = tx.commit("fold git").await.map_err(engine_err)?;
        self.repo = repo;
        self.jj
            .check_out(self.repo.op_id().clone(), None, &line)
            .await
            .map_err(engine_err)?;
        Ok(GitFold::Folded {
            head: line.id().hex(),
        })
    }

    /// Keep imported refs without moving the line, as the fold's
    /// no-movement outcome.
    async fn absorb_refs(&mut self, tx: Transaction, fence: Fence<'_>) -> Result<GitFold, Error> {
        fence()?;
        self.repo = tx.commit("fold git refs").await.map_err(engine_err)?;
        Ok(GitFold::Current)
    }

    /// Whether the moved history between `target` and its merge base
    /// with the line holds a commit carrying the line's exact tree —
    /// the line survives a history rewrite whole, so its own diff must
    /// not be replayed against it. Pre-divergence history never counts:
    /// any commit inside the merge bases' own ancestry is pruned, so an
    /// ancient tree that merely resembles today's line cannot
    /// fast-forward away current work. The scan walks an explicit
    /// stack, bounded by [`FOLD_SCAN_MAX`]; past the cap the fold falls
    /// back to a merge.
    async fn line_content_in(
        &self,
        tx: &Transaction,
        wc_commit: &jj_lib::commit::Commit,
        target_id: &CommitId,
    ) -> Result<bool, Error> {
        let bases = tx
            .repo()
            .index()
            .common_ancestors(
                std::slice::from_ref(wc_commit.id()),
                std::slice::from_ref(target_id),
            )
            .map_err(engine_err)?;
        let mut seen = BTreeSet::new();
        let mut pending = vec![target_id.clone()];
        'walk: while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if seen.len() > FOLD_SCAN_MAX {
                return Ok(false);
            }
            for base in &bases {
                if is_ancestor(tx, &id, base).await? {
                    continue 'walk;
                }
            }
            let commit = self.repo.store().get_commit(&id).map_err(engine_err)?;
            if commit.tree_ids() == wc_commit.tree_ids() {
                return Ok(true);
            }
            pending.extend(commit.parent_ids().iter().cloned());
        }
        Ok(false)
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
        let git = resolve_git_identity()?;
        let settings = build_settings(actor, git.as_ref())?;
        let (mut jj, repo) = JjWorkspace::init_external_git(&settings, root, &root.join(".git"))
            .await
            .map_err(engine_err)?;
        let mut tx = repo.start_transaction();
        git::import_head(tx.repo_mut()).await.map_err(engine_err)?;
        git::import_refs(tx.repo_mut(), &import_options())
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
                    .set_author(stamp(author_identity(actor, git.as_ref())))
                    // The continuation lands beneath the first landed
                    // change; `git log` must not read it as a blank.
                    .set_description("adopt")
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
            author: author_identity(actor, git.as_ref()),
            git,
        })
    }

    /// Snapshot outstanding edits. Records a new commit only when the tree
    /// changed; returns the new snapshot id in that case.
    pub fn snapshot(&mut self) -> Result<Option<String>, Error> {
        block_on(self.snapshot_with(&SnapshotStyle::Stack, None))
    }

    /// Snapshot outstanding edits like [`Engine::snapshot`], under a
    /// leased line move: the fence runs before the snapshot's git HEAD
    /// write and operation commit, so a superseded holder records
    /// nothing.
    pub fn snapshot_fenced(&mut self, fence: Fence<'_>) -> Result<Option<String>, Error> {
        block_on(self.snapshot_with(&SnapshotStyle::Stack, Some(fence)))
    }

    /// Snapshot outstanding edits like [`Engine::snapshot`], but leave
    /// the colocated git HEAD alone: the fold that follows reads the
    /// out-of-band HEAD as a possible target, and resetting it here
    /// would erase the move (or refuse against it) before the import.
    pub fn snapshot_keep_head(&mut self, fence: Fence<'_>) -> Result<Option<String>, Error> {
        block_on(self.snapshot_with(&SnapshotStyle::StackKeepHead, Some(fence)))
    }

    /// Snapshot outstanding edits by amending this workspace's commit: the
    /// session's change id survives while its tree advances.
    pub fn snapshot_amend(&mut self) -> Result<Option<String>, Error> {
        block_on(self.snapshot_with(&SnapshotStyle::Amend, None))
    }

    async fn snapshot_with(
        &mut self,
        style: &SnapshotStyle,
        fence: Option<Fence<'_>>,
    ) -> Result<Option<String>, Error> {
        let name = self.jj.workspace_name().to_owned();
        let wc_id = match self.repo.view().get_wc_commit_id(&name) {
            Some(id) => id.clone(),
            None => return Err(Error::Engine("no working-copy commit".to_owned())),
        };
        let options = snapshot_options(base_ignores(&self.boundary)?);
        let author = self.author();

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
        let new_commit =
            Self::write_snapshot_commit(tx.repo_mut(), style, wc_id, &wc_commit, new_tree, author)
                .await?;
        let new_id = new_commit.id().clone();
        tx.repo_mut()
            .set_wc_commit(name, new_id.clone())
            .map_err(engine_err)?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(engine_err)?;
        // A leased snapshot fences at the last pure moment — before the
        // git HEAD write a Stack style performs, and before the commit
        // either way; unleased snapshots record disk truth and carry no
        // tenancy.
        if let Some(fence) = fence {
            fence()?;
        }
        // Stack moves a shared line: keep the colocated git HEAD on it so
        // plain git sees what jj wrote. The fold's pre-snapshot leaves
        // HEAD to the fold; Amend is a session's and never steals HEAD.
        match style {
            SnapshotStyle::Stack => {
                git::reset_head(tx.repo_mut(), &new_commit)
                    .await
                    .map_err(engine_err)?;
            }
            SnapshotStyle::StackKeepHead | SnapshotStyle::Amend => {}
        }
        let repo = tx.commit("snapshot").await.map_err(engine_err)?;
        locked
            .finish(repo.op_id().clone())
            .await
            .map_err(engine_err)?;
        self.repo = repo;
        Ok(Some(new_id.hex()))
    }

    /// Write the commit a snapshot style records: a stack state on the
    /// line, or a session change's amend. A stack state names itself so
    /// `git log` never reads a blank; an amend keeps the change's own
    /// description.
    async fn write_snapshot_commit(
        repo: &mut MutableRepo,
        style: &SnapshotStyle,
        wc_id: CommitId,
        wc_commit: &jj_lib::commit::Commit,
        new_tree: MergedTree,
        author: Signature,
    ) -> Result<jj_lib::commit::Commit, Error> {
        match style {
            SnapshotStyle::Stack | SnapshotStyle::StackKeepHead => repo
                .new_commit(vec![wc_id], new_tree)
                .set_author(author)
                .set_description("snapshot")
                .write()
                .await
                .map_err(engine_err),
            SnapshotStyle::Amend => repo
                .rewrite_commit(wc_commit)
                .set_tree(new_tree)
                .write()
                .await
                .map_err(engine_err),
        }
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
    /// shared head and a fresh change there, authored by `actor` and
    /// described by `description` — the landed git commit's message. The
    /// new change's id.
    pub fn create_session_workspace(
        &mut self,
        root: &Path,
        name: &str,
        actor: &Actor,
        description: &str,
    ) -> Result<String, Error> {
        block_on(self.create_session_workspace_async(root, name, actor, description))
    }

    async fn create_session_workspace_async(
        &mut self,
        root: &Path,
        name: &str,
        actor: &Actor,
        description: &str,
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
            .set_author(stamp(author_identity(actor, self.git.as_ref())))
            .set_description(description)
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
    pub fn land(
        &mut self,
        tip: &str,
        bookmark: &str,
        fence: Fence<'_>,
    ) -> Result<LandOutcome, Error> {
        block_on(self.land_async(tip, bookmark, fence))
    }

    async fn land_async(
        &mut self,
        tip: &str,
        bookmark: &str,
        fence: Fence<'_>,
    ) -> Result<LandOutcome, Error> {
        let name = self.jj.workspace_name().to_owned();
        let head_id = self.wc_commit_id()?;
        let tip_commit = self.commit_at(tip)?;
        let mut tx = self.repo.start_transaction();
        let rebased = rebase_commit(tx.repo_mut(), tip_commit, vec![head_id])
            .await
            .map_err(engine_err)?;
        if rebased.has_conflict() {
            // Parking is a gate decision too: a superseded holder must
            // not record a conflict judged from a stale head.
            fence()?;
            return Ok(LandOutcome::Conflicted);
        }
        tx.repo_mut()
            .set_wc_commit(name, rebased.id().clone())
            .map_err(engine_err)?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(engine_err)?;
        // Descendant rebasing can run long; the fence sits after it, at
        // the last pure moment before the first git ref write.
        fence()?;
        // The line advanced; keep the colocated git HEAD on it. A move
        // failure here is a mid-apply race — git moved between the fold
        // and this write; retrying folds the move first.
        git::reset_head(tx.repo_mut(), &rebased).await.map_err(|error| {
            Error::Engine(format!(
                "git HEAD moved during the apply ({error}); rerun the landing — the next operation folds the move first"
            ))
        })?;
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
    pub fn step_back(
        &mut self,
        landed: &str,
        bookmark: &str,
        fence: Fence<'_>,
    ) -> Result<StepBack, Error> {
        block_on(self.step_back_async(landed, bookmark, fence))
    }

    async fn step_back_async(
        &mut self,
        landed: &str,
        bookmark: &str,
        fence: Fence<'_>,
    ) -> Result<StepBack, Error> {
        let name = self.jj.workspace_name().to_owned();
        let head = self.wc_commit_id()?;
        let landed_commit = self.commit_at(landed)?;
        let Some(parent_id) = landed_commit.parent_ids().first() else {
            return Err(Error::Engine(format!(
                "the landed snapshot {landed} has no parent to step back to"
            )));
        };
        // The outcome itself is consumed — `AlreadyStepped` makes the
        // caller delete the landing record — so a superseded holder must
        // not classify the line from a stale tenancy.
        fence()?;
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
        // never publishes the undone head as the newest state. A move
        // failure here is a mid-undo race; retrying folds the move first.
        git::reset_head(tx.repo_mut(), &parent).await.map_err(|error| {
            Error::Engine(format!(
                "git HEAD moved during the undo ({error}); rerun the undo — the next operation folds the move first"
            ))
        })?;
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

/// A fresh working-copy state for a workspace the repo already records:
/// initialized empty at the repo's head operation, so the check-out that
/// follows writes the recorded tree whole.
fn init_absent_working_copy(
    repo: &Arc<ReadonlyRepo>,
    root: &Path,
    jj_dir: &Path,
    name: WorkspaceNameBuf,
) -> Result<Box<dyn WorkingCopy>, Error> {
    let state_path = jj_dir.join("working_copy");
    fs::create_dir(&state_path)?;
    let factory = default_working_copy_factory();
    let working_copy = factory
        .init_working_copy(
            repo.store().clone(),
            root.to_path_buf(),
            state_path.clone(),
            repo.op_id().clone(),
            name,
            repo.settings(),
        )
        .map_err(engine_err)?;
    fs::write(state_path.join("type"), working_copy.name())?;
    Ok(working_copy)
}

/// The jj settings an engine runs under. The `user` — author and
/// committer of every commit the engine writes — is the publishing
/// identity when one is configured, else a synthetic per-actor address.
/// Configured signing signs with behavior `force`: everything this
/// engine writes publishes under the identity's key, whoever authored
/// it — agents author, the owner vouches (ADR-0015).
fn build_settings(actor: &Actor, git: Option<&GitIdentity>) -> Result<UserSettings, Error> {
    #[derive(serde::Serialize)]
    struct UserConfig<'a> {
        user: UserSection<'a>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signing: Option<SigningSection<'a>>,
    }

    #[derive(serde::Serialize)]
    struct UserSection<'a> {
        name: &'a str,
        email: &'a str,
    }

    #[derive(serde::Serialize)]
    struct SigningSection<'a> {
        backend: &'static str,
        behavior: &'static str,
        key: &'a str,
    }

    let synthesized;
    let (name, email) = if let Some(git) = git {
        (git.name.as_str(), git.email.as_str())
    } else {
        synthesized = format!("{}@atelier.local", actor.name);
        (actor.name.as_str(), synthesized.as_str())
    };
    let signing = git
        .and_then(|git| git.signing.as_ref())
        .map(|signing| SigningSection {
            backend: match signing.backend {
                SigningBackend::Gpg => "gpg",
                SigningBackend::Ssh => "ssh",
            },
            behavior: "force",
            key: &signing.key,
        });

    let mut config = StackedConfig::with_defaults();
    let text = toml::to_string(&UserConfig {
        user: UserSection { name, email },
        signing,
    })
    .map_err(config_err)?;
    let layer = ConfigLayer::parse(ConfigSource::User, &text).map_err(config_err)?;
    config.add_layer(layer);
    UserSettings::from_config(config).map_err(config_err)
}

/// The author an actor's commits carry: the publishing identity for the
/// owning human; the synthetic actor address for agents and automations,
/// so their work stays attributed while the committer — and, when
/// signing is configured, the signature — remains the identity's
/// (ADR-0015).
fn author_identity(actor: &Actor, git: Option<&GitIdentity>) -> (String, String) {
    match (git, actor.kind) {
        (Some(git), ActorKind::Human) => (git.name.clone(), git.email.clone()),
        (Some(_) | None, ActorKind::Agent | ActorKind::Automation) | (None, ActorKind::Human) => {
            (actor.name.clone(), format!("{}@atelier.local", actor.name))
        }
    }
}

/// A signature stamped now from an author identity.
fn stamp((name, email): (String, String)) -> Signature {
    Signature {
        name,
        email,
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

/// Copy `source` into `target` exactly as a snapshot would version it
/// (ADR-0002: adopt, never import — and never adopt what snapshotting
/// would skip). The walk is the snapshot's own walk: per-directory
/// `.gitignore` chains, nested repositories skipped whole (jj#4349 — a
/// directory holding a `.git` is not this working copy's content), and
/// the engine-internal names at any depth. The one deliberate widening:
/// the adoption copy also carries `target/.git` itself — the repository
/// is the content being adopted. `keep_git` at the root only; a nested
/// repo's `.git` is never ours to copy.
pub(crate) fn copy_versioned_tree(
    source: &Path,
    target: &Path,
    keep_git: bool,
) -> Result<(), Error> {
    let mut pending = vec![(
        source.to_path_buf(),
        target.to_path_buf(),
        RepoPathBuf::root(),
        GitIgnoreFile::empty(),
    )];
    while let Some((from_dir, to_dir, dir, ignores)) = pending.pop() {
        let ignores = ignores
            .chain_with_file(&dir, from_dir.join(".gitignore"))
            .map_err(engine_err)?;
        for entry in fs::read_dir(&from_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if SKIP_NAMES.contains(&name)
                && !(keep_git && dir == RepoPathBuf::root() && name == ".git")
            {
                continue;
            }
            let Ok(component) = RepoPathComponent::new(name) else {
                continue;
            };
            let path = dir.join(component);
            let from = entry.path();
            let to = to_dir.join(name);
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                // A nested repository is skipped whole, as the snapshot
                // skips it: its state belongs to its own repository, and
                // copying it would version a checkout whose git state
                // stays behind.
                let nested = RESERVED_DIR_NAMES
                    .iter()
                    .any(|reserved| from.join(reserved).symlink_metadata().is_ok());
                if nested {
                    continue;
                }
                if ignores.matches_dir(&path) {
                    continue;
                }
                pending.push((from, to, path, ignores.clone()));
            } else if file_type.is_symlink() {
                // A symlink versions as a symlink; `fs::copy` would
                // follow it and flatten the link into a file.
                if ignores.matches_file(&path) {
                    continue;
                }
                let link = fs::read_link(&from)?;
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                std::os::unix::fs::symlink(&link, &to)?;
            } else {
                if ignores.matches_file(&path) {
                    continue;
                }
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&from, &to)?;
            }
        }
    }
    Ok(())
}

/// The names a working copy reserves, as the snapshot walk reserves them.
const RESERVED_DIR_NAMES: &[&str] = &[".git", ".jj"];

/// The commit an out-of-band move put the line's target on: `branch`'s
/// when it names one, git HEAD's otherwise.
fn line_target(tx: &mut Transaction, branch: &str) -> Option<CommitId> {
    match tx
        .repo_mut()
        .view()
        .get_local_bookmark(RefName::new(branch))
        .as_normal()
        .cloned()
    {
        Some(id) => Some(id),
        None => tx.repo_mut().view().git_head().as_normal().cloned(),
    }
}

/// Whether the imported `descendant` builds on `ancestor`, judged by the
/// transaction's index, which already carries the imported commits.
async fn is_ancestor(
    tx: &Transaction,
    ancestor: &CommitId,
    descendant: &CommitId,
) -> Result<bool, Error> {
    tx.repo()
        .index()
        .is_ancestor(ancestor, descendant)
        .await
        .map_err(engine_err)
}

/// How atelier imports git refs: adopted history stays reachable, no
/// remote bookmarks auto-track — mounts publish with plain `git push`,
/// never through jj's remote machinery.
fn import_options() -> GitImportOptions {
    GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: std::collections::HashMap::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActorKind;

    fn actor() -> Actor {
        Actor {
            name: "fence-test".to_owned(),
            kind: ActorKind::Agent,
        }
    }

    /// A refused fence publishes nothing, deterministically: the refusal
    /// lands before the step writes, before the outcome classifies as
    /// `AlreadyStepped` — which the caller consumes to erase a landing
    /// record — and a later permitted rerun still steps the line.
    #[test]
    fn a_refused_fence_neither_steps_nor_classifies() {
        let root = tempfile::tempdir().unwrap();
        let mut engine = Engine::init(root.path(), &actor(), &[]).unwrap();
        std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
        let first = engine.snapshot().unwrap().expect("first snapshot");
        std::fs::write(root.path().join("a.txt"), "two\n").unwrap();
        let landed = engine.snapshot().unwrap().expect("landed snapshot");

        let refuse: Fence<'_> = &|| {
            Err(Error::LeaseSuperseded {
                point: "landing".to_owned(),
            })
        };
        let error = engine.step_back(&landed, "atelier", refuse).unwrap_err();
        assert!(
            matches!(error, Error::LeaseSuperseded { .. }),
            "got: {error:?}"
        );

        let permit: Fence<'_> = &|| Ok(());
        let step = engine.step_back(&landed, "atelier", permit).unwrap();
        assert!(
            matches!(step, StepBack::Stepped { ref restored } if *restored == first),
            "the refusal must have left the line unstepped, got: {step:?}"
        );

        // The stepped line classifies as already stepped — but only for a
        // holder whose tenancy still stands.
        let error = engine.step_back(&landed, "atelier", refuse).unwrap_err();
        assert!(
            matches!(error, Error::LeaseSuperseded { .. }),
            "got: {error:?}"
        );
        let step = engine.step_back(&landed, "atelier", permit).unwrap();
        assert!(matches!(step, StepBack::AlreadyStepped), "got: {step:?}");
    }
}
