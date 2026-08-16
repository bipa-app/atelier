use std::env::{self, VarError};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atelier_diff_core::{
    Address, Delta, DeltaKind, Diff, Fidelity, FormatPackage, PackageId, as_text, detect_package,
    diff_lines,
};
use atelier_format_docx::DocxPackage;
use notify::{Event, RecursiveMode, Watcher};

use crate::config::{
    Actor, InstructionFidelity, ROOT_MOUNT, Source, SourceKind, SyncPolicy, WorkspaceConfig,
    read_workspace_config, resolve_actor, write_workspace_config,
};
use crate::coordination::{Coordination, LeaseClaim, RequestRow, SessionRow};
use crate::engine::{DiffSides, Engine, FileBlob, LandOutcome, MAX_LADDER_FILE_SIZE, Side};
use crate::error::{Error, config_err};
use crate::journal::{Act, Journal, JournalEntry};
use crate::landing::{Approval, GateOutcome, Landing, LandingRequest, RequestId, RequestState};
use crate::projection::ProjectionCache;
use crate::read::{ReadResult, window_size, window_text};
use crate::session::{Instruction, Session, SessionId, SessionState, SourceChange};
use crate::watch::{
    STOP_TICK, WatchEvent, WatchStop, event_is_content, settle, watcher_failed, watcher_gone,
};

pub use crate::engine::Snapshot;

const CONTROL_DIR: &str = ".atelier";
const JOURNAL_FILE: &str = "journal.sqlite3";
const SESSIONS_DIR: &str = "sessions";
pub(crate) const SKIP_NAMES: [&str; 3] = [".atelier", ".jj", ".git"];

/// The one scarce point of a workspace in v1: its landing point.
const LANDING_LEASE_POINT: &str = "landing";
/// The bookmark a landing moves when no adopted branch names one; exported
/// as a git branch so plain `git push` carries the shared line.
const LANDED_BOOKMARK: &str = "atelier";
/// How long a landing lease lives; a holder that dies mid-apply frees the
/// point when this passes.
const LANDING_LEASE_TTL_MS: i64 = 30_000;

/// A named, versioned body of work content with its own histories and
/// journal. The root engine is source zero; each mounted source carries
/// its own engine and history (ADR-0009).
pub struct Workspace {
    root: PathBuf,
    actor: Actor,
    engine: Engine,
    /// Mounted sources in mount-name order — the deterministic order every
    /// aggregate read model and the landing fan-out walk them in.
    mounts: Vec<MountedSource>,
    journal: Journal,
    coordination: Coordination,
    packages: Vec<Box<dyn FormatPackage>>,
    projections: ProjectionCache,
}

/// One mounted source: its name and the engine carrying its history.
struct MountedSource {
    name: String,
    engine: Engine,
    /// The adopted branch landings move; `None` falls back to
    /// [`LANDED_BOOKMARK`].
    branch: Option<String>,
}

impl Workspace {
    /// Turn `path` into a workspace: control dir, journal, and engine store.
    pub fn init(path: impl AsRef<Path>) -> Result<Self, Error> {
        let root = path.as_ref().to_path_buf();
        let actor = resolve_actor()?;

        let control = root.join(CONTROL_DIR);
        if control.exists() {
            return Err(Error::WorkspaceExists(root));
        }
        if let Some(ancestor) = enclosing_workspace(&root) {
            return Err(Error::NestedWorkspace(ancestor));
        }

        fs::create_dir_all(&control)?;
        let engine = Engine::init(&root, &actor, &[])?;

        let config = WorkspaceConfig::new(workspace_name(&root));
        write_workspace_config(&control, &config)?;

        let journal = Journal::open(&control.join(JOURNAL_FILE))?;
        let coordination = Coordination::open(&control.join(JOURNAL_FILE))?;
        let mounts = Vec::new();
        let workspace = Self {
            root,
            actor,
            engine,
            mounts,
            journal,
            coordination,
            packages: builtin_packages(),
            projections: ProjectionCache::new(&control),
        };
        let entry = workspace.entry(Act::WorkspaceInit, None)?;
        workspace.journal.append(&entry)?;
        Ok(workspace)
    }

    /// Open the workspace already present at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let root = path.as_ref().to_path_buf();
        let actor = resolve_actor()?;

        let control = root.join(CONTROL_DIR);
        if !control.exists() {
            return Err(Error::NotAWorkspace(root));
        }

        let config = read_workspace_config(&control)?;
        let mount_names = mount_names(&config);
        let engine = Engine::open(&root, &actor, &mount_names)?;
        let mut mounts = Vec::new();
        for name in &mount_names {
            let branch = config
                .sources
                .iter()
                .find(|source| source.mount == *name)
                .and_then(|source| source.branch.clone());
            mounts.push(MountedSource {
                name: name.clone(),
                engine: Engine::open(&root.join(name), &actor, &[])?,
                branch,
            });
        }
        let journal = Journal::open(&control.join(JOURNAL_FILE))?;
        let coordination = Coordination::open(&control.join(JOURNAL_FILE))?;
        Ok(Self {
            root,
            actor,
            engine,
            mounts,
            journal,
            coordination,
            packages: builtin_packages(),
            projections: ProjectionCache::new(&control),
        })
    }

    /// The actor this workspace handle acts as.
    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// Attach a local folder, importing its content into the root — source
    /// zero. One root import per workspace; mounted sources go through
    /// [`Workspace::attach_mount`].
    pub fn attach(&mut self, folder: impl AsRef<Path>) -> Result<Source, Error> {
        let folder = folder.as_ref();
        if !folder.is_dir() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("source folder not found: {}", folder.display()),
            )));
        }

        let control = self.root.join(CONTROL_DIR);
        let mut config = read_workspace_config(&control)?;
        if config.sources.iter().any(|s| s.mount == ROOT_MOUNT) {
            return Err(Error::AlreadyAttached);
        }
        if folder_uses_lfs(folder)? {
            return Err(Error::LfsSourceUnsupported);
        }

        self.auto_snapshot()?;

        copy_tree(folder, &self.root)?;
        let source = Source {
            kind: SourceKind::LocalFolder,
            path: folder.to_path_buf(),
            sync: SyncPolicy::TwoWay,
            mount: ROOT_MOUNT.to_owned(),
            branch: None,
        };
        config.sources.push(source.clone());
        write_workspace_config(&control, &config)?;

        let snapshot = self.engine.snapshot()?;
        let entry = self.entry(Act::SourceAttach, snapshot)?;
        self.journal.append(&entry)?;
        Ok(source)
    }

    /// Attach a local folder as a mounted source: its own engine, its own
    /// history, at `root/<name>` (ADR-0009).
    pub fn attach_mount(&mut self, folder: impl AsRef<Path>, name: &str) -> Result<Source, Error> {
        let folder = folder.as_ref();
        if !folder.is_dir() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("source folder not found: {}", folder.display()),
            )));
        }
        valid_mount_name(name)?;
        let control = self.root.join(CONTROL_DIR);
        let mut config = read_workspace_config(&control)?;
        if config.sources.iter().any(|s| s.mount == name) {
            return Err(Error::AlreadyAttached);
        }
        let mount_dir = self.root.join(name);
        if mount_dir.exists() {
            return Err(Error::Config(format!(
                "mount {name:?} collides with existing workspace content"
            )));
        }
        if folder_uses_lfs(folder)? {
            return Err(Error::LfsSourceUnsupported);
        }

        // Settle every engine before the boundary moves.
        self.auto_snapshot()?;

        fs::create_dir_all(&mount_dir)?;
        // A folder that is already a git repository is adopted, never
        // imported: its history is preserved and the mount stays a real
        // repo plain git pushes (ADR-0009).
        let adopts_git = folder.join(".git").is_dir();
        let (kind, mut engine) = if adopts_git {
            copy_tree_with_git(folder, &mount_dir)?;
            (
                SourceKind::LocalGit,
                Engine::adopt_git(&mount_dir, &self.actor, &[])?,
            )
        } else {
            let engine = Engine::init(&mount_dir, &self.actor, &[])?;
            copy_tree(folder, &mount_dir)?;
            (SourceKind::LocalFolder, engine)
        };
        let snapshot = engine.snapshot()?;
        // The branch is read from the source itself: the engine detaches
        // the copy's HEAD as lines move, so only the origin's HEAD names
        // what the source had checked out.
        let branch = match kind {
            SourceKind::LocalGit => adopted_branch(folder)?,
            SourceKind::LocalFolder => None,
        };

        let source = Source {
            kind,
            path: folder.to_path_buf(),
            sync: SyncPolicy::TwoWay,
            mount: name.to_owned(),
            branch: branch.clone(),
        };
        config.sources.push(source.clone());
        write_workspace_config(&control, &config)?;

        // The root engine's boundary now excludes the new mount; reopen it
        // so its ignores see the world as configured.
        let mount_names = mount_names(&config);
        self.engine = Engine::open(&self.root, &self.actor, &mount_names)?;
        let position = self
            .mounts
            .binary_search_by(|mount| mount.name.as_str().cmp(name))
            .unwrap_or_else(|position| position);
        self.mounts.insert(
            position,
            MountedSource {
                name: name.to_owned(),
                engine,
                branch,
            },
        );

        let reference = snapshot.map(|id| format!("{name} {id}"));
        let entry = self.entry(Act::SourceAttach, reference)?;
        self.journal.append(&entry)?;
        Ok(source)
    }

    /// The shared lines' snapshots: the root's, then each mount's in name
    /// order, each newest first, `limit` applying per source.
    pub fn log(&mut self, limit: usize) -> Result<Vec<SourceSnapshot>, Error> {
        self.refresh_engines()?;
        self.auto_snapshot()?;
        let mut entries = Vec::new();
        for snapshot in self.engine.log(limit)? {
            entries.push(SourceSnapshot {
                source: None,
                snapshot,
            });
        }
        for mount in &self.mounts {
            for snapshot in mount.engine.log(limit)? {
                entries.push(SourceSnapshot {
                    source: Some(mount.name.clone()),
                    snapshot,
                });
            }
        }
        Ok(entries)
    }

    /// Diff each source's latest snapshot against its first parent, root
    /// first then mounts in name order, every delta raised to the highest
    /// rung the ladder allows and mounted addresses scoped by mount.
    pub fn diff_latest(&mut self) -> Result<Diff, Error> {
        self.refresh_engines()?;
        self.auto_snapshot()?;
        let (diff, sides) = self.engine.diff_latest()?;
        let mut deltas = self.raised(&self.engine, diff, &sides, None)?.deltas;
        for mount in &self.mounts {
            let (mount_diff, sides) = mount.engine.diff_latest()?;
            let raised = self.raised(&mount.engine, mount_diff, &sides, Some(&mount.name))?;
            deltas.extend(raised.deltas);
        }
        Ok(Diff { deltas })
    }

    /// Render the read model an actor consumes first: identity, sources,
    /// discipline, live state, and the loop this workspace expects. Every
    /// face returns this text verbatim (ADR-0006: one render, three faces).
    pub fn manifest(&mut self) -> Result<String, Error> {
        self.refresh_engines()?;
        self.auto_snapshot()?;
        let config = read_workspace_config(&self.root.join(CONTROL_DIR))?;
        let mut lines = vec![
            format!("workspace: {}", config.workspace.name),
            format!("schema: {}", config.schema),
            String::new(),
            "sources:".to_owned(),
        ];
        if config.sources.is_empty() {
            lines.push("  (none)".to_owned());
        }
        for source in &config.sources {
            lines.push(format!(
                "  {}  {}  {}  {}",
                source.mount,
                source.kind,
                source.path.display(),
                source.sync
            ));
        }
        lines.push(String::new());
        lines.push("discipline:".to_owned());
        let landing = config.landing;
        let self_approve = if landing.allow_self_approve {
            "allowed"
        } else {
            "forbidden"
        };
        let dismiss = if landing.dismiss_approvals_on_new_snapshots {
            "yes"
        } else {
            "no"
        };
        lines.push(format!(
            "  approvals: {}  self-approval: {self_approve}  snapshots dismiss approvals: {dismiss}",
            landing.approvals
        ));
        let fidelity = match config.journal.instruction_fidelity {
            InstructionFidelity::Summary => "summary",
            InstructionFidelity::Verbatim => "verbatim",
        };
        lines.push(format!("  instructions: {fidelity}"));
        lines.push(String::new());
        lines.push("state:".to_owned());
        lines.push(format!("  head: {}", self.engine.head()?));
        for mount in &self.mounts {
            lines.push(format!("  head {}: {}", mount.name, mount.engine.head()?));
        }
        let mut open_sessions: Vec<String> = self
            .sessions()?
            .into_iter()
            .filter(|session| match session.state {
                SessionState::Open => true,
                SessionState::Landed | SessionState::Abandoned => false,
            })
            .map(|session| session.id.to_string())
            .collect();
        open_sessions.reverse();
        lines.push(if open_sessions.is_empty() {
            "  open sessions: none".to_owned()
        } else {
            format!("  open sessions: {}", open_sessions.join(", "))
        });
        let mut live_requests: Vec<String> = self
            .landing_requests()?
            .into_iter()
            .filter(|request| match request.state {
                RequestState::Open | RequestState::Approved | RequestState::Parked => true,
                RequestState::Landed | RequestState::Rejected | RequestState::Abandoned => false,
            })
            .map(|request| format!("{} ({})", request.id, request.state))
            .collect();
        live_requests.reverse();
        lines.push(if live_requests.is_empty() {
            "  live requests: none".to_owned()
        } else {
            format!("  live requests: {}", live_requests.join(", "))
        });
        lines.push(String::new());
        lines.push("the loop:".to_owned());
        lines
            .push("  open_session -> write -> diff -> land (or request_land + approve)".to_owned());
        lines.push(
            "  mount-scoped paths address sources; editing never takes the landing lease"
                .to_owned(),
        );
        Ok(lines.join("\n"))
    }

    /// Diff two of the root line's snapshots by id: `before` against
    /// `after`, each delta raised to the highest rung the ladder allows.
    /// Mounted lines' snapshot pairs arrive with the session fan-out.
    pub fn diff_between(&mut self, before: &str, after: &str) -> Result<Diff, Error> {
        self.refresh_engines()?;
        self.auto_snapshot()?;
        let (diff, sides) = self.engine.diff_between(before, after)?;
        self.raised(&self.engine, diff, &sides, None)
    }

    /// Read up to `limit` journal entries, newest first.
    pub fn journal(&mut self, limit: usize) -> Result<Vec<JournalEntry>, Error> {
        self.refresh_engines()?;
        self.auto_snapshot()?;
        self.journal.entries(limit)
    }

    /// Open a session for `actor`: its own working copy holding the shared
    /// head, its own change. Isolation is not optional — every session
    /// starts isolated, and only landing serializes.
    pub fn open_session(
        &mut self,
        actor: &Actor,
        instruction: &Instruction,
    ) -> Result<Session, Error> {
        self.refresh_engines()?;
        self.auto_snapshot()?;
        let verbatim = match self.config()?.journal.instruction_fidelity {
            InstructionFidelity::Summary => None,
            InstructionFidelity::Verbatim => instruction.verbatim.clone(),
        };
        let row = self.coordination.create_session(
            actor,
            &instruction.summary,
            instruction.run_ref.as_deref(),
            verbatim.as_deref(),
            now_ms()?,
        )?;
        let id = SessionId(row);
        let change_id = match self.engine.create_session_workspace(
            &self.session_root(id),
            &format!("session-{id}"),
            actor,
        ) {
            Ok(change_id) => change_id,
            Err(error) => {
                self.coordination.delete_session(row)?;
                return Err(error);
            }
        };
        self.coordination.set_session_change(row, &change_id)?;
        // The session spans every source: one working copy and one change
        // per mount, mirroring the workspace's shape (ADR-0009).
        let session_root = self.session_root(id);
        for index in 0..self.mounts.len() {
            let name = self.mounts[index].name.clone();
            let mount_change = match self.mounts[index].engine.create_session_workspace(
                &session_root.join(&name),
                &format!("session-{id}"),
                actor,
            ) {
                Ok(change_id) => change_id,
                Err(error) => {
                    self.coordination.delete_session(row)?;
                    return Err(error);
                }
            };
            self.coordination
                .set_session_source_change(row, &name, &mount_change)?;
        }
        self.journal.append(&JournalEntry {
            at_ms: now_ms()?,
            actor_name: actor.name.clone(),
            actor_kind: actor.kind,
            act: Act::SessionOpen,
            session: Some(id.to_string()),
            instruction_summary: Some(instruction.summary.clone()),
            instruction_run_ref: instruction.run_ref.clone(),
            instruction_verbatim: verbatim,
            reference: None,
        })?;
        self.session(id)
    }

    /// Every session, newest first. Sessions are durable rows plus real
    /// directories: they survive process restarts, and nothing deletes them.
    pub fn sessions(&mut self) -> Result<Vec<Session>, Error> {
        let rows = self.coordination.sessions()?;
        rows.into_iter().map(|row| self.session_from(row)).collect()
    }

    /// The session named `id`.
    pub fn session(&mut self, id: SessionId) -> Result<Session, Error> {
        match self.coordination.session(id.0)? {
            Some(row) => self.session_from(row),
            None => Err(Error::SessionNotFound(id.to_string())),
        }
    }

    /// Write `content` at `path` inside the session's working copy — a
    /// mount-scoped path lands in that source's working copy — and
    /// snapshot every source; the id of the written source's tip snapshot.
    pub fn session_write(
        &mut self,
        id: SessionId,
        path: &str,
        content: &str,
    ) -> Result<String, Error> {
        self.engine.refresh()?;
        let session = self.open_session_only(id)?;
        let (source, directory, inner) = self.session_target(&session, path);
        let file = session_file(&directory, &inner)?;
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file, content)?;
        let tips = self.snapshot_session(&session)?;
        Ok(tips.tip_of(source.as_deref()))
    }

    /// Read `path` inside the session's working copy, windowed. A document
    /// a package projects reads as its projection; plain text reads as
    /// itself; anything else refuses — raw byte views arrive with a later
    /// slice.
    pub fn session_read(
        &mut self,
        id: SessionId,
        path: &str,
        start: usize,
        max_bytes: Option<usize>,
    ) -> Result<ReadResult, Error> {
        let session = self.open_session_only(id)?;
        let size = window_size(max_bytes)?;
        let (_, directory, inner) = self.session_target(&session, path);
        let file = session_file(&directory, &inner)?;
        let bytes = fs::read(&file)?;
        if let Some(package) = self.detected(path, &bytes)? {
            let text = self.project_for_read(package, &bytes)?;
            return Ok(window_text(&text, start, size, Some(package.id())));
        }
        match as_text(&bytes) {
            Some(text) => Ok(window_text(text, start, size, None)),
            None => Err(Error::NotText(path.to_owned())),
        }
    }

    /// Each source's session change against the shared-line snapshot it
    /// forked from, raised through the ladder like any diff, mounted
    /// addresses scoped by mount. An untouched source contributes nothing.
    pub fn session_diff(&mut self, id: SessionId) -> Result<Diff, Error> {
        self.refresh_engines()?;
        let session = self.open_session_only(id)?;
        let tips = self.snapshot_session(&session)?;
        let base = self.engine.parent_of(&tips.root)?;
        let (diff, sides) = self.engine.diff_between(&base, &tips.root)?;
        let mut deltas = self.raised(&self.engine, diff, &sides, None)?.deltas;
        for (name, tip) in &tips.mounts {
            let mount = self.mount(name)?;
            let base = mount.engine.parent_of(tip)?;
            let (diff, sides) = mount.engine.diff_between(&base, tip)?;
            let raised = self.raised(&mount.engine, diff, &sides, Some(name))?;
            deltas.extend(raised.deltas);
        }
        Ok(Diff { deltas })
    }

    /// Open the session's landing request — the gate's object, never a
    /// direct write (ADR-0007). Asking again returns the request already
    /// holding the gate.
    pub fn request_land(&mut self, id: SessionId) -> Result<LandingRequest, Error> {
        self.refresh_engines()?;
        let session = self.open_session_only(id)?;
        self.snapshot_session(&session)?;
        if let Some(row) = self.coordination.gated_request_for_session(id.0)? {
            return self.request_from(row);
        }
        let row = self
            .coordination
            .create_request(id.0, &session.actor, now_ms()?)?;
        let request_id = RequestId(row);
        self.append_session_entry(
            &session.actor,
            Act::LandRequest,
            id,
            Some(request_id.to_string()),
        )?;
        self.request(request_id)
    }

    /// Every landing request, newest first.
    pub fn landing_requests(&mut self) -> Result<Vec<LandingRequest>, Error> {
        let rows = self.coordination.requests()?;
        rows.into_iter().map(|row| self.request_from(row)).collect()
    }

    /// The landing request named `id`.
    pub fn request(&mut self, id: RequestId) -> Result<LandingRequest, Error> {
        match self.coordination.request(id.0)? {
            Some(row) => self.request_from(row),
            None => Err(Error::RequestNotFound(id.to_string())),
        }
    }

    /// Record `approver`'s approval on the request; when the gate is
    /// satisfied the apply runs — lease, rebase, advance — landing the
    /// change or parking the request on a conflict.
    pub fn approve(&mut self, id: RequestId, approver: &Actor) -> Result<GateOutcome, Error> {
        self.refresh_engines()?;
        let row = self.gated_request(id)?;
        let session = self.open_session_only(SessionId(row.session_id))?;
        let policy = self.config()?.landing;
        let requester = Actor {
            name: row.requester_name.clone(),
            kind: row.requester_kind,
        };
        if !policy.allow_self_approve && *approver == requester {
            return Err(Error::SelfApprovalForbidden);
        }
        let tips = self.snapshot_session(&session)?;
        // The approval covers the change as its root tip names it; a new
        // snapshot on any source dismisses approvals through the gate's
        // side effects, so a stale approval never carries later work.
        let tip = tips.root.clone();
        // The snapshot may have dismissed approvals and re-opened the gate;
        // judge the gate on what the store holds now.
        let row = self.gated_request(id)?;
        if let RequestState::Open = row.state {
            self.coordination
                .add_approval(row.id, approver, &tip, now_ms()?)?;
            self.append_session_entry(
                approver,
                Act::Approve,
                session.id,
                Some(format!("{id} {tip}")),
            )?;
        }
        let approvals = self.coordination.live_approvals(row.id)?;
        let approvers: std::collections::BTreeSet<(&str, &str)> = approvals
            .iter()
            .map(|approval| (approval.actor_name.as_str(), approval.actor_kind.as_str()))
            .collect();
        if (approvers.len() as u64) < u64::from(policy.approvals) {
            return Ok(GateOutcome::Pending {
                request: self.request(id)?,
                required: policy.approvals,
            });
        }
        // The gate was judged satisfied on Open; another process may have
        // moved the request since. Losing the move means re-judging, not
        // overwriting: an already-approved request proceeds to its apply,
        // a closed one refuses by name through the re-fetch above.
        if !self.coordination.move_request_state(
            row.id,
            &[RequestState::Open],
            RequestState::Approved,
        )? {
            let row = self.gated_request(id)?;
            if let RequestState::Open = row.state {
                // The gate re-opened (a new snapshot dismissed approvals):
                // this approval no longer satisfies it.
                return Ok(GateOutcome::Pending {
                    request: self.request(id)?,
                    required: policy.approvals,
                });
            }
        }
        self.apply(&session, id, &tips, approver)
    }

    /// Reject the request: the gate closes, the session stays open.
    pub fn reject(
        &mut self,
        id: RequestId,
        actor: &Actor,
        reason: Option<&str>,
    ) -> Result<LandingRequest, Error> {
        // A rejection closes a gate still deciding: Open or Approved.
        // Losing the move means the gate settled first — refuse by name.
        let row = self.gated_request(id)?;
        while !self.coordination.move_request_state(
            row.id,
            &[RequestState::Open, RequestState::Approved],
            RequestState::Rejected,
        )? {
            self.gated_request(id)?;
        }
        let reference = match reason {
            Some(reason) => format!("{id} {reason}"),
            None => id.to_string(),
        };
        self.append_session_entry(
            actor,
            Act::Reject,
            SessionId(row.session_id),
            Some(reference),
        )?;
        self.request(id)
    }

    /// Land the session's change: sugar for request plus self-approval.
    /// Where policy forbids self-approval the request stays pending for
    /// other approvers.
    pub fn land(&mut self, id: SessionId) -> Result<GateOutcome, Error> {
        let request = self.request_land(id)?;
        let session = self.session(id)?;
        let policy = self.config()?.landing;
        if !policy.allow_self_approve {
            return Ok(GateOutcome::Pending {
                request,
                required: policy.approvals,
            });
        }
        self.approve(request.id, &session.actor)
    }

    /// Close the session without landing; its work stays in history and
    /// its working copy stays on disk.
    pub fn abandon(&mut self, id: SessionId) -> Result<Session, Error> {
        self.engine.refresh()?;
        let session = self.open_session_only(id)?;
        self.snapshot_session(&session)?;
        let mut reference = None;
        if let Some(request) = self.coordination.gated_request_for_session(id.0)? {
            // Abandonment closes any still-gated request; losing the move
            // means the gate settled concurrently (landed or rejected),
            // and that outcome stands — the session still closes.
            let _ = self.coordination.move_request_state(
                request.id,
                &[
                    RequestState::Open,
                    RequestState::Approved,
                    RequestState::Parked,
                ],
                RequestState::Abandoned,
            )?;
            reference = Some(RequestId(request.id).to_string());
        }
        if !self.coordination.move_session_state(
            id.0,
            SessionState::Open,
            SessionState::Abandoned,
        )? {
            // A concurrent apply landed the session between the open check
            // above and this write; the landing stands.
            let session = self.session(id)?;
            return Err(Error::SessionClosed {
                id: id.to_string(),
                state: session.state.to_string(),
            });
        }
        self.append_session_entry(&session.actor, Act::SessionAbandon, id, reference)?;
        self.session(id)
    }

    /// Snapshot outstanding edits in every engine — root first, mounts in
    /// name order — through the one snapshot path every operation shares;
    /// each recorded snapshot with the source that took it.
    fn auto_snapshot(&mut self) -> Result<Vec<(Option<String>, String)>, Error> {
        let mut recorded = Vec::new();
        if let Some(id) = self.engine.snapshot()? {
            let entry = self.entry(Act::Snapshot, Some(id.clone()))?;
            self.journal.append(&entry)?;
            recorded.push((None, id));
        }
        for mount in &mut self.mounts {
            if let Some(id) = mount.engine.snapshot()? {
                recorded.push((Some(mount.name.clone()), id));
            }
        }
        for (mount, id) in &recorded {
            if let Some(mount) = mount {
                let entry = self.entry(Act::Snapshot, Some(format!("{mount} {id}")))?;
                self.journal.append(&entry)?;
            }
        }
        Ok(recorded)
    }

    /// Reload every engine at its current operation head, folding in what
    /// other processes committed since this handle loaded.
    fn refresh_engines(&mut self) -> Result<(), Error> {
        self.engine.refresh()?;
        for mount in &mut self.mounts {
            mount.engine.refresh()?;
        }
        Ok(())
    }

    /// Watch the workspace root: external edits become attributed
    /// snapshots through the same snapshot path every operation uses.
    /// Blocks until `stop` asks it to return; edits made while no watcher
    /// runs are caught up by the scan at start. Each snapshot — and the
    /// armed watcher itself — reaches the caller through `on_event`.
    pub fn watch(
        &mut self,
        debounce: Duration,
        mut on_event: impl FnMut(&WatchEvent),
        stop: &WatchStop,
    ) -> Result<(), Error> {
        // notify reports canonical paths; the filter's prefix check needs
        // the root in the same form.
        let root = fs::canonicalize(&self.root)?;
        let (pulses, storm) = std::sync::mpsc::channel();
        let filter_root = root.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: Result<Event, notify::Error>| {
                let pulse = match event {
                    Ok(event) => {
                        if !event_is_content(&filter_root, &event) {
                            return;
                        }
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                // A send after the loop returned has no listener; that is the
                // watcher being dropped, not a failure.
                let _ = pulses.send(pulse);
            })
            .map_err(|error| watcher_failed(&error))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|error| watcher_failed(&error))?;
        on_event(&WatchEvent::Started);
        self.snapshot_watched(&mut on_event)?;
        while !stop.stopped() {
            match storm.recv_timeout(STOP_TICK) {
                Ok(Ok(())) => {
                    settle(&storm, debounce, stop)?;
                    self.snapshot_watched(&mut on_event)?;
                }
                Ok(Err(error)) => return Err(watcher_failed(&error)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(watcher_gone()),
            }
        }
        Ok(())
    }

    /// One watched snapshot: fold in operations other processes committed,
    /// then snapshot; a recorded snapshot reaches the watcher's caller.
    fn snapshot_watched(&mut self, on_event: &mut impl FnMut(&WatchEvent)) -> Result<(), Error> {
        self.refresh_engines()?;
        for (_, snapshot) in self.auto_snapshot()? {
            on_event(&WatchEvent::Snapshotted { snapshot });
        }
        Ok(())
    }

    /// The gate-satisfied apply, fanned out per source (ADR-0009): the
    /// root first, then mounts in name order; each touched line takes its
    /// own lease, rebases, and advances — or parks. A landing already
    /// recorded for this request is never repeated, so a retry after a
    /// park or a lost lease finishes what remains. Editing never takes a
    /// lease; only landing does.
    fn apply(
        &mut self,
        session: &Session,
        id: RequestId,
        tips: &SessionTips,
        approver: &Actor,
    ) -> Result<GateOutcome, Error> {
        let already = self.coordination.landings(id.0)?;
        let mut parked = Vec::new();
        // The root always lands — the v1 line, even when untouched, so a
        // zero-mount workspace keeps its exact behavior. Mounts land only
        // when their session change carries work.
        let mut plan: Vec<(Option<String>, String)> = vec![(None, tips.root.clone())];
        for (name, tip) in &tips.mounts {
            if self.mount(name)?.engine.tree_changed(tip)? {
                plan.push((Some(name.clone()), tip.clone()));
            }
        }
        for (source, tip) in plan {
            if already.iter().any(|(landed, _)| *landed == source) {
                continue;
            }
            match self.apply_source(session, id, source.as_deref(), &tip, approver)? {
                LandOutcome::Landed { .. } => {}
                LandOutcome::Conflicted => parked.push(source),
            }
        }
        let landings: Vec<Landing> = self
            .coordination
            .landings(id.0)?
            .into_iter()
            .map(|(source, snapshot)| Landing { source, snapshot })
            .collect();
        if parked.is_empty() {
            // Losing the request move means the gate re-opened for a newer
            // snapshot or the session was abandoned mid-apply — the
            // winner's state stands and the session stays open for its
            // remaining work; the landings are recorded either way.
            if self.coordination.move_request_state(
                id.0,
                &[RequestState::Approved],
                RequestState::Landed,
            )? {
                let _ = self.coordination.move_session_state(
                    session.id.0,
                    SessionState::Open,
                    SessionState::Landed,
                )?;
            }
            return Ok(GateOutcome::Landed { landings });
        }
        // A parked line closes the gate until a new snapshot; what landed
        // stands (ADR-0009 — never pretended atomicity).
        let _ = self.coordination.move_request_state(
            id.0,
            &[RequestState::Approved],
            RequestState::Parked,
        )?;
        Ok(GateOutcome::Parked {
            request: self.request(id)?,
            landings,
            parked,
        })
    }

    /// Land one source's tip under its own lease; the outcome of that one
    /// line. The landing journals and records with its source, so nothing
    /// repeats it and nothing mistakes it for another line's.
    fn apply_source(
        &mut self,
        session: &Session,
        id: RequestId,
        source: Option<&str>,
        tip: &str,
        approver: &Actor,
    ) -> Result<LandOutcome, Error> {
        let point = match source {
            Some(name) => format!("{LANDING_LEASE_POINT}/{name}"),
            None => LANDING_LEASE_POINT.to_owned(),
        };
        let holder = format!("{}:{}", self.actor.name, std::process::id());
        let now = now_ms()?;
        match self
            .coordination
            .claim_lease(&point, &holder, now, LANDING_LEASE_TTL_MS)?
        {
            LeaseClaim::HeldByOther {
                holder,
                expires_at_ms,
            } => {
                return Err(Error::LeaseHeld {
                    holder,
                    expires_at_ms,
                });
            }
            LeaseClaim::Held => {}
        }
        let outcome = self.apply_source_holding_lease(session, id, source, tip, approver);
        let released = self.coordination.release_lease(&point, &holder);
        let outcome = outcome?;
        released?;
        Ok(outcome)
    }

    fn apply_source_holding_lease(
        &mut self,
        session: &Session,
        id: RequestId,
        source: Option<&str>,
        tip: &str,
        approver: &Actor,
    ) -> Result<LandOutcome, Error> {
        // Test seam: the cross-process lease test needs the winner to hold
        // the point long enough for the loser to observe `LeaseHeld`.
        if let Some(hold) = land_hold_ms()? {
            std::thread::sleep(Duration::from_millis(hold));
        }
        // Another process may have advanced this line since the gate
        // check; the lease is held, so the head stays put through the
        // apply.
        self.refresh_engines()?;
        self.auto_snapshot()?;
        let outcome = match source {
            None => self.engine.land(tip, LANDED_BOOKMARK)?,
            Some(name) => {
                let index = self
                    .mounts
                    .iter()
                    .position(|mount| mount.name == name)
                    .ok_or_else(|| Error::Engine(format!("no source is mounted at {name:?}")))?;
                let bookmark = self.mounts[index]
                    .branch
                    .clone()
                    .unwrap_or_else(|| LANDED_BOOKMARK.to_owned());
                self.mounts[index].engine.land(tip, &bookmark)?
            }
        };
        let scoped = |text: &str| match source {
            Some(name) => format!("{name} {text}"),
            None => text.to_owned(),
        };
        match &outcome {
            LandOutcome::Conflicted => {
                self.append_session_entry(
                    approver,
                    Act::LandParked,
                    session.id,
                    Some(scoped(&id.to_string())),
                )?;
            }
            LandOutcome::Landed { snapshot } => {
                self.coordination.record_landing(id.0, source, snapshot)?;
                self.append_session_entry(
                    approver,
                    Act::Land,
                    session.id,
                    Some(scoped(&format!("{id} {snapshot}"))),
                )?;
            }
        }
        Ok(outcome)
    }

    /// Snapshot every source's session working copy; each source's tip.
    /// A new snapshot — on any source — is journaled and runs the gate's
    /// side effects: it dismisses approvals (policy-decided) and re-opens
    /// an approved or parked request.
    fn snapshot_session(&mut self, session: &Session) -> Result<SessionTips, Error> {
        // The session's root working copy shares the root's boundary: a
        // mount name never lands on the shared line as root content.
        let boundary = self.mount_boundary();
        let mut engine = Engine::open(&session.working_copy, &session.actor, &boundary)?;
        let new_snapshot = engine.snapshot_amend()?;
        let root_tip = engine.head()?;
        let mut recorded: Vec<(Option<String>, String)> = Vec::new();
        if let Some(new_snapshot) = new_snapshot {
            recorded.push((None, new_snapshot));
        }
        let mut mounts = Vec::new();
        for name in boundary {
            let mut engine = Engine::open(&session.working_copy.join(&name), &session.actor, &[])?;
            if let Some(new_snapshot) = engine.snapshot_amend()? {
                recorded.push((Some(name.clone()), new_snapshot));
            }
            mounts.push((name, engine.head()?));
        }
        for (source, new_snapshot) in &recorded {
            let reference = match source {
                Some(source) => format!("{source} {new_snapshot}"),
                None => new_snapshot.clone(),
            };
            self.append_session_entry(&session.actor, Act::Snapshot, session.id, Some(reference))?;
            self.gate_reacts_to_snapshot(session, new_snapshot)?;
        }
        if !recorded.is_empty() {
            // The landing engines read this handle's view; fold the
            // sessions' operations in.
            self.refresh_engines()?;
        }
        Ok(SessionTips {
            root: root_tip,
            mounts,
        })
    }

    /// The mounted source called `name`.
    fn mount(&self, name: &str) -> Result<&MountedSource, Error> {
        self.mounts
            .iter()
            .find(|mount| mount.name == name)
            .ok_or_else(|| Error::Engine(format!("no source is mounted at {name:?}")))
    }

    /// Where a session path lives: the mount whose name leads it, or the
    /// session's root working copy.
    fn session_target(&self, session: &Session, path: &str) -> (Option<String>, PathBuf, String) {
        if let Some((first, rest)) = path.split_once('/')
            && !rest.is_empty()
            && self.mounts.iter().any(|mount| mount.name == first)
        {
            return (
                Some(first.to_owned()),
                session.working_copy.join(first),
                rest.to_owned(),
            );
        }
        (None, session.working_copy.clone(), path.to_owned())
    }

    fn gate_reacts_to_snapshot(
        &mut self,
        session: &Session,
        new_snapshot: &str,
    ) -> Result<(), Error> {
        let Some(request) = self.coordination.gated_request_for_session(session.id.0)? else {
            return Ok(());
        };
        let id = RequestId(request.id);
        match request.state {
            RequestState::Open | RequestState::Approved | RequestState::Parked => {
                if self.config()?.landing.dismiss_approvals_on_new_snapshots {
                    let dismissed = self.coordination.dismiss_approvals(request.id)?;
                    if dismissed > 0 {
                        self.append_session_entry(
                            &session.actor,
                            Act::ApprovalsDismissed,
                            session.id,
                            Some(format!("{id} {new_snapshot}")),
                        )?;
                    }
                }
                match request.state {
                    // A new snapshot re-opens the gate: an approved apply
                    // no longer covers the change, a parked conflict may
                    // now be resolved. Losing the move means the gate
                    // closed concurrently — a closed gate stays closed.
                    RequestState::Approved | RequestState::Parked => {
                        let _ = self.coordination.move_request_state(
                            request.id,
                            &[RequestState::Approved, RequestState::Parked],
                            RequestState::Open,
                        )?;
                    }
                    RequestState::Open
                    | RequestState::Landed
                    | RequestState::Rejected
                    | RequestState::Abandoned => {}
                }
            }
            RequestState::Landed | RequestState::Rejected | RequestState::Abandoned => {}
        }
        Ok(())
    }

    /// The request while its gate is still deciding; closed states refuse
    /// by name, and a parked request points at its way back (a new
    /// snapshot).
    fn gated_request(&mut self, id: RequestId) -> Result<RequestRow, Error> {
        let Some(row) = self.coordination.request(id.0)? else {
            return Err(Error::RequestNotFound(id.to_string()));
        };
        match row.state {
            RequestState::Open | RequestState::Approved => Ok(row),
            RequestState::Parked => Err(Error::RequestParked(id.to_string())),
            RequestState::Landed | RequestState::Rejected | RequestState::Abandoned => {
                Err(Error::RequestClosed {
                    id: id.to_string(),
                    state: row.state.to_string(),
                })
            }
        }
    }

    /// The session when it is still open for work.
    fn open_session_only(&mut self, id: SessionId) -> Result<Session, Error> {
        let session = self.session(id)?;
        match session.state {
            SessionState::Open => Ok(session),
            SessionState::Landed | SessionState::Abandoned => Err(Error::SessionClosed {
                id: id.to_string(),
                state: session.state.to_string(),
            }),
        }
    }

    fn session_from(&self, row: SessionRow) -> Result<Session, Error> {
        let id = SessionId(row.id);
        let change_id = row.change_id.ok_or_else(|| {
            Error::Engine(format!("session {id} has no change; its bootstrap failed"))
        })?;
        let mut changes = vec![SourceChange {
            source: None,
            change_id: change_id.clone(),
        }];
        for (source, mount_change) in self.coordination.session_source_changes(row.id)? {
            changes.push(SourceChange {
                source: Some(source),
                change_id: mount_change,
            });
        }
        Ok(Session {
            id,
            actor: Actor {
                name: row.actor_name,
                kind: row.actor_kind,
            },
            state: row.state,
            change_id,
            changes,
            working_copy: self.session_root(id),
            instruction_summary: row.instruction_summary,
            instruction_run_ref: row.instruction_run_ref,
            opened_at_ms: row.opened_at_ms,
        })
    }

    fn request_from(&self, row: RequestRow) -> Result<LandingRequest, Error> {
        let approvals = self
            .coordination
            .live_approvals(row.id)?
            .into_iter()
            .map(|approval| Approval {
                actor: Actor {
                    name: approval.actor_name,
                    kind: approval.actor_kind,
                },
                snapshot: approval.snapshot_id,
                at_ms: approval.at_ms,
            })
            .collect();
        Ok(LandingRequest {
            id: RequestId(row.id),
            session_id: SessionId(row.session_id),
            requester: Actor {
                name: row.requester_name,
                kind: row.requester_kind,
            },
            state: row.state,
            approvals,
            created_at_ms: row.created_at_ms,
        })
    }

    fn session_root(&self, id: SessionId) -> PathBuf {
        self.root
            .join(CONTROL_DIR)
            .join(SESSIONS_DIR)
            .join(id.to_string())
    }

    fn config(&self) -> Result<WorkspaceConfig, Error> {
        read_workspace_config(&self.root.join(CONTROL_DIR))
    }

    fn append_session_entry(
        &self,
        actor: &Actor,
        act: Act,
        session: SessionId,
        reference: Option<String>,
    ) -> Result<(), Error> {
        self.journal.append(&JournalEntry {
            at_ms: now_ms()?,
            actor_name: actor.name.clone(),
            actor_kind: actor.kind,
            act,
            session: Some(session.to_string()),
            instruction_summary: None,
            instruction_run_ref: None,
            instruction_verbatim: None,
            reference,
        })
    }

    /// The document's projection for a read: the cache entry when
    /// published, computed and published otherwise. A read has no lower
    /// rung to fall to, so a failing or panicking package errors.
    fn project_for_read(&self, package: &dyn FormatPackage, bytes: &[u8]) -> Result<String, Error> {
        let blob = FileBlob {
            id: crate::projection::content_id(bytes),
            bytes: bytes.to_vec(),
        };
        if let Some(text) = self.projections.read(package.id(), &blob) {
            return Ok(text);
        }
        match catch_unwind(AssertUnwindSafe(|| package.project(&blob.bytes))) {
            Ok(Ok(projection)) => {
                // As in the diff path: the projection is already computed,
                // so a failed publish must not gate the read.
                let _ = self
                    .projections
                    .store(package.id(), &blob, &projection.text);
                Ok(projection.text)
            }
            Ok(Err(error)) => Err(Error::PackageFailed {
                package: package.id().to_string(),
                reason: error.to_string(),
            }),
            Err(_) => Err(Error::PackageFailed {
                package: package.id().to_string(),
                reason: "the package panicked during projection".to_owned(),
            }),
        }
    }

    /// Raise every delta the ladder can: through a package projection when
    /// one detects the document, as plain text when both sides are text,
    /// else leave it at the binary rung it arrived at. A package differ's
    /// rich deltas follow the file delta they refine. Deltas from a
    /// mounted source carry mount-scoped addresses.
    fn raised(
        &self,
        engine: &Engine,
        diff: Diff,
        sides: &DiffSides,
        mount: Option<&str>,
    ) -> Result<Diff, Error> {
        let mut deltas = Vec::new();
        for delta in diff.deltas {
            deltas.extend(self.raise(engine, delta, sides, mount)?);
        }
        Ok(Diff { deltas })
    }

    /// Only `Changed` deltas raise in v1: an added or removed document is
    /// already told by its listing line, without dumping its whole content.
    fn raise(
        &self,
        engine: &Engine,
        delta: Delta,
        sides: &DiffSides,
        mount: Option<&str>,
    ) -> Result<Vec<Delta>, Error> {
        // The engine addresses files by the path inside its own world; the
        // delta the workspace reports scopes that path by mount, and every
        // journal entry below speaks the scoped address.
        let raw = delta.address.as_str().to_owned();
        let mut delta = delta;
        if let Some(mount) = mount {
            delta.address = Address::new(format!("{mount}/{raw}"));
        }
        if delta.kind != DeltaKind::Changed {
            return Ok(vec![delta]);
        }
        let (before, after) = match engine.read_file_sides(sides, &raw)? {
            (Side::Blob(before), Side::Blob(after)) => (before, after),
            (Side::TooLarge, _) | (_, Side::TooLarge) => {
                self.file_too_large(delta.address.as_str())?;
                return Ok(vec![delta]);
            }
            (Side::Absent, _) | (_, Side::Absent) => return Ok(vec![delta]),
        };
        if let Some(package) = self.detected(delta.address.as_str(), &after.bytes)? {
            let projections = (
                self.projection(package, delta.address.as_str(), &before)?,
                self.projection(package, delta.address.as_str(), &after)?,
            );
            let (Some(projected_before), Some(projected_after)) = projections else {
                return Ok(vec![delta]);
            };
            let raised = delta.at_text_rung(
                diff_lines(&projected_before, &projected_after),
                Some(package.id()),
            );
            return self.enriched(raised, package, &before, &after);
        }
        // "Fidelity drops to text or binary" (CONTEXT.md, Format Package):
        // a package-less document that decodes as text diffs as text —
        // content-based detection, the git model — because extension
        // allowlists would drop the source and config files agents edit
        // all day to the binary rung. Opaque bytes stay binary.
        match (as_text(&before.bytes), as_text(&after.bytes)) {
            (Some(before), Some(after)) => {
                Ok(vec![delta.at_text_rung(diff_lines(before, after), None)])
            }
            _ => Ok(vec![delta]),
        }
    }

    /// The Rich rung, additive over the text rung: the package differ's
    /// deltas — formatting the projection cannot express — follow the file
    /// delta, their format-terms addresses scoped under its path. Text
    /// changes stay on the file delta's lines, so nothing the differ does
    /// not model can ever drop out of a diff. A failing or panicking
    /// differ journals `package_failed` and the text rung stands.
    fn enriched(
        &self,
        raised: Delta,
        package: &dyn FormatPackage,
        before: &FileBlob,
        after: &FileBlob,
    ) -> Result<Vec<Delta>, Error> {
        let rich = match catch_unwind(AssertUnwindSafe(|| {
            package.diff(&before.bytes, &after.bytes)
        })) {
            Ok(None) => return Ok(vec![raised]),
            Ok(Some(Ok(rich))) => rich,
            Ok(Some(Err(error))) => {
                self.differ_failed(raised.address.as_str(), package.id(), &error.to_string())?;
                return Ok(vec![raised]);
            }
            Err(_) => {
                self.differ_failed(
                    raised.address.as_str(),
                    package.id(),
                    "the package panicked during diffing",
                )?;
                return Ok(vec![raised]);
            }
        };
        if rich.is_empty() {
            return Ok(vec![raised]);
        }
        let path = raised.address.as_str().to_owned();
        let mut deltas = vec![Delta {
            fidelity: Fidelity::Rich,
            ..raised
        }];
        deltas.extend(rich.into_iter().map(|delta| Delta {
            address: Address::new(format!("{path} > {}", delta.address.as_str())),
            ..delta
        }));
        Ok(deltas)
    }

    /// The package claiming the document, behind a panic boundary: a
    /// panicking package degrades fidelity, it never kills the process
    /// (its journal entry keeps the degradation loud).
    fn detected(&self, address: &str, bytes: &[u8]) -> Result<Option<&dyn FormatPackage>, Error> {
        if let Ok(package) = catch_unwind(AssertUnwindSafe(|| {
            detect_package(&self.packages, address, bytes)
        })) {
            Ok(package)
        } else {
            self.package_failed(address, None, "a package panicked during detection")?;
            Ok(None)
        }
    }

    /// One side's projection: the cache entry when published, computed and
    /// published otherwise. `None` when the package failed or panicked —
    /// journaled as `package_failed`, so the delta's fall to the binary
    /// rung is never silent.
    fn projection(
        &self,
        package: &dyn FormatPackage,
        address: &str,
        blob: &FileBlob,
    ) -> Result<Option<String>, Error> {
        if let Some(text) = self.projections.read(package.id(), blob) {
            return Ok(Some(text));
        }
        match catch_unwind(AssertUnwindSafe(|| package.project(&blob.bytes))) {
            Ok(Ok(projection)) => {
                // The cache is derived and evictable: the projection is
                // already computed and correct, so a failed publish must
                // not gate the diff — it only costs a recomputation on
                // some later diff.
                let _ = self.projections.store(package.id(), blob, &projection.text);
                Ok(Some(projection.text))
            }
            Ok(Err(error)) => {
                self.package_failed(address, Some(package.id()), &error.to_string())?;
                Ok(None)
            }
            Err(_) => {
                self.package_failed(
                    address,
                    Some(package.id()),
                    "the package panicked during projection",
                )?;
                Ok(None)
            }
        }
    }

    fn package_failed(
        &self,
        address: &str,
        package: Option<PackageId>,
        reason: &str,
    ) -> Result<(), Error> {
        let reference = match package {
            Some(id) => format!("{address} {id} fell_back_to=binary: {reason}"),
            None => format!("{address} fell_back_to=binary: {reason}"),
        };
        let entry = self.entry(Act::PackageFailed, Some(reference))?;
        self.journal.append(&entry)
    }

    /// A differ failure costs only the rich rung: the text rung the
    /// projection already raised stands, and the journal keeps the
    /// degradation loud.
    fn differ_failed(&self, address: &str, package: PackageId, reason: &str) -> Result<(), Error> {
        let reference = format!("{address} {package} fell_back_to=text: {reason}");
        let entry = self.entry(Act::PackageFailed, Some(reference))?;
        self.journal.append(&entry)
    }

    /// A file past the ladder cap keeps its binary-rung listing line; the
    /// journal records the degradation so it is never silent.
    fn file_too_large(&self, address: &str) -> Result<(), Error> {
        let reference = format!(
            "{address} exceeds the {MAX_LADDER_FILE_SIZE}-byte ladder cap; kept at the binary rung"
        );
        let entry = self.entry(Act::FileTooLarge, Some(reference))?;
        self.journal.append(&entry)
    }

    fn entry(&self, act: Act, reference: Option<String>) -> Result<JournalEntry, Error> {
        Ok(JournalEntry {
            at_ms: now_ms()?,
            actor_name: self.actor.name.clone(),
            actor_kind: self.actor.kind,
            act,
            session: None,
            instruction_summary: None,
            instruction_run_ref: None,
            instruction_verbatim: None,
            reference,
        })
    }

    /// The root engine's boundary: every mount name, in name order.
    fn mount_boundary(&self) -> Vec<String> {
        self.mounts.iter().map(|mount| mount.name.clone()).collect()
    }
}

/// Every format package built into this core, in detection order.
fn builtin_packages() -> Vec<Box<dyn FormatPackage>> {
    vec![Box::new(DocxPackage)]
}

/// Each source's session tip: the root's, and every mount's by name.
struct SessionTips {
    root: String,
    mounts: Vec<(String, String)>,
}

impl SessionTips {
    /// The tip of `source` — the root's when `None`. A session always has
    /// a tip for every source it spans.
    fn tip_of(&self, source: Option<&str>) -> String {
        match source {
            None => self.root.clone(),
            Some(name) => self
                .mounts
                .iter()
                .find(|(mount, _)| mount == name)
                .map_or_else(|| self.root.clone(), |(_, tip)| tip.clone()),
        }
    }
}

/// One snapshot in one source's history: the root's when `source` is
/// `None`, else the named mount's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub source: Option<String>,
    pub snapshot: Snapshot,
}

/// The mounted sources a config names, in name order — the one order every
/// aggregate walks.
fn mount_names(config: &WorkspaceConfig) -> Vec<String> {
    let mut names: Vec<String> = config
        .sources
        .iter()
        .filter(|source| source.mount != ROOT_MOUNT)
        .map(|source| source.mount.clone())
        .collect();
    names.sort();
    names
}

/// A mount name is one path component that cannot collide with engine
/// internals or escape the root.
fn valid_mount_name(name: &str) -> Result<(), Error> {
    let flat = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\');
    if !flat || SKIP_NAMES.contains(&name) {
        return Err(Error::Config(format!(
            "mount name {name:?} must be one path component outside the engine's internals"
        )));
    }
    Ok(())
}

fn workspace_name(root: &Path) -> String {
    match root.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.to_owned(),
        None => "workspace".to_owned(),
    }
}

fn enclosing_workspace(root: &Path) -> Option<PathBuf> {
    let mut current = root.parent();
    while let Some(dir) = current {
        if dir.join(CONTROL_DIR).exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// The branch the adopted repository has checked out: the symbolic ref in
/// the source's `.git/HEAD`, `None` for a detached head. Landings move this
/// branch so plain `git push` from the mount carries the shared line.
fn adopted_branch(source: &Path) -> Result<Option<String>, Error> {
    let head = fs::read_to_string(source.join(".git").join("HEAD"))?;
    Ok(head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned))
}
fn folder_uses_lfs(folder: &Path) -> Result<bool, Error> {
    let gitattributes = folder.join(".gitattributes");
    if !gitattributes.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(&gitattributes)?;
    Ok(text.contains("filter=lfs"))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if SKIP_NAMES.iter().any(|skip| name == **skip) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            fs::create_dir_all(&to)?;
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Copy a source tree keeping its `.git` — the adoption path needs the
/// repository itself, not just its files. `.atelier` and `.jj` still stay
/// behind: they are engine internals, never source content.
fn copy_tree_with_git(src: &Path, dst: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".atelier" || name == ".jj" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            fs::create_dir_all(&to)?;
            copy_tree_with_git(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The file at `path` inside `working_copy`: a relative path that never
/// climbs out — parent and root components refuse.
fn session_file(working_copy: &Path, path: &str) -> Result<PathBuf, Error> {
    let relative = Path::new(path);
    let stays_inside = relative.components().all(|component| match component {
        Component::Normal(_) | Component::CurDir => true,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    });
    if path.is_empty() || !stays_inside {
        return Err(Error::PathOutsideWorkingCopy(path.to_owned()));
    }
    Ok(working_copy.join(relative))
}

/// The `ATELIER_LAND_HOLD_MS` test seam, absent in normal runs; a set but
/// unparsable value refuses instead of silently not holding.
fn land_hold_ms() -> Result<Option<u64>, Error> {
    match env::var("ATELIER_LAND_HOLD_MS") {
        Ok(value) => value.parse().map(Some).map_err(config_err),
        Err(VarError::NotPresent) => Ok(None),
        Err(error @ VarError::NotUnicode(_)) => Err(config_err(error)),
    }
}

fn now_ms() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(config_err)?;
    i64::try_from(elapsed.as_millis()).map_err(config_err)
}
