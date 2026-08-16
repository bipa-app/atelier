use std::env::{self, VarError};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atelier_diff_core::{
    Delta, DeltaKind, Diff, FormatPackage, PackageId, as_text, detect_package, diff_lines,
};
use atelier_format_docx::DocxPackage;

use crate::config::{
    Actor, InstructionFidelity, Source, SourceKind, SyncPolicy, WorkspaceConfig,
    read_workspace_config, resolve_actor, write_workspace_config,
};
use crate::coordination::{Coordination, LeaseClaim, RequestRow, SessionRow};
use crate::engine::{DiffSides, Engine, FileBlob, LandOutcome, MAX_LADDER_FILE_SIZE, Side};
use crate::error::{Error, config_err};
use crate::journal::{Act, Journal, JournalEntry};
use crate::landing::{Approval, GateOutcome, LandingRequest, RequestId, RequestState};
use crate::projection::ProjectionCache;
use crate::read::{ReadResult, window_size, window_text};
use crate::session::{Instruction, Session, SessionId, SessionState};

pub use crate::engine::Snapshot;

const CONTROL_DIR: &str = ".atelier";
const JOURNAL_FILE: &str = "journal.sqlite3";
const SESSIONS_DIR: &str = "sessions";
const SKIP_NAMES: [&str; 3] = [".atelier", ".jj", ".git"];

/// The one scarce point of a workspace in v1: its landing point.
const LANDING_LEASE_POINT: &str = "landing";
/// How long a landing lease lives; a holder that dies mid-apply frees the
/// point when this passes.
const LANDING_LEASE_TTL_MS: i64 = 30_000;

/// A named, versioned body of work content with its own history and journal.
pub struct Workspace {
    root: PathBuf,
    actor: Actor,
    engine: Engine,
    journal: Journal,
    coordination: Coordination,
    packages: Vec<Box<dyn FormatPackage>>,
    projections: ProjectionCache,
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
        let engine = Engine::init(&root, &actor)?;

        let config = WorkspaceConfig::new(workspace_name(&root));
        write_workspace_config(&control, &config)?;

        let journal = Journal::open(&control.join(JOURNAL_FILE))?;
        let coordination = Coordination::open(&control.join(JOURNAL_FILE))?;
        let workspace = Self {
            root,
            actor,
            engine,
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

        let engine = Engine::open(&root, &actor)?;
        let journal = Journal::open(&control.join(JOURNAL_FILE))?;
        let coordination = Coordination::open(&control.join(JOURNAL_FILE))?;
        Ok(Self {
            root,
            actor,
            engine,
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

    /// Attach the one local-folder source, importing its content.
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
        if !config.sources.is_empty() {
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
            mount: PathBuf::from("/"),
        };
        config.sources.push(source.clone());
        write_workspace_config(&control, &config)?;

        let snapshot = self.engine.snapshot()?;
        let entry = self.entry(Act::SourceAttach, snapshot)?;
        self.journal.append(&entry)?;
        Ok(source)
    }

    /// The ancestor chain of the latest snapshot, newest first.
    pub fn log(&mut self, limit: usize) -> Result<Vec<Snapshot>, Error> {
        self.engine.refresh()?;
        self.auto_snapshot()?;
        self.engine.log(limit)
    }

    /// Diff the latest snapshot against its first parent, each delta raised
    /// to the highest rung the ladder allows.
    pub fn diff_latest(&mut self) -> Result<Diff, Error> {
        self.engine.refresh()?;
        self.auto_snapshot()?;
        let (diff, sides) = self.engine.diff_latest()?;
        self.raised(diff, &sides)
    }

    /// Diff two snapshots by id: `before` against `after`, each delta
    /// raised to the highest rung the ladder allows.
    pub fn diff_between(&mut self, before: &str, after: &str) -> Result<Diff, Error> {
        self.engine.refresh()?;
        self.auto_snapshot()?;
        let (diff, sides) = self.engine.diff_between(before, after)?;
        self.raised(diff, &sides)
    }

    /// Read up to `limit` journal entries, newest first.
    pub fn journal(&mut self, limit: usize) -> Result<Vec<JournalEntry>, Error> {
        self.engine.refresh()?;
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
        self.engine.refresh()?;
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

    /// Write `content` at `path` inside the session's working copy and
    /// snapshot it; the id of the session's tip snapshot.
    pub fn session_write(
        &mut self,
        id: SessionId,
        path: &str,
        content: &str,
    ) -> Result<String, Error> {
        self.engine.refresh()?;
        let session = self.open_session_only(id)?;
        let file = session_file(&session.working_copy, path)?;
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file, content)?;
        self.snapshot_session(&session)
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
        let file = session_file(&session.working_copy, path)?;
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

    /// The session's change against the shared-line snapshot it forked
    /// from, raised through the ladder like any diff.
    pub fn session_diff(&mut self, id: SessionId) -> Result<Diff, Error> {
        self.engine.refresh()?;
        let session = self.open_session_only(id)?;
        let tip = self.snapshot_session(&session)?;
        let base = self.engine.parent_of(&tip)?;
        let (diff, sides) = self.engine.diff_between(&base, &tip)?;
        self.raised(diff, &sides)
    }

    /// Open the session's landing request — the gate's object, never a
    /// direct write (ADR-0007). Asking again returns the request already
    /// holding the gate.
    pub fn request_land(&mut self, id: SessionId) -> Result<LandingRequest, Error> {
        self.engine.refresh()?;
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
        self.engine.refresh()?;
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
        let tip = self.snapshot_session(&session)?;
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
        self.coordination
            .set_request_state(row.id, RequestState::Approved)?;
        self.apply(&session, id, &tip, approver)
    }

    /// Reject the request: the gate closes, the session stays open.
    pub fn reject(
        &mut self,
        id: RequestId,
        actor: &Actor,
        reason: Option<&str>,
    ) -> Result<LandingRequest, Error> {
        let row = self.gated_request(id)?;
        self.coordination
            .set_request_state(row.id, RequestState::Rejected)?;
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
            self.coordination
                .set_request_state(request.id, RequestState::Abandoned)?;
            reference = Some(RequestId(request.id).to_string());
        }
        self.coordination
            .set_session_state(id.0, SessionState::Abandoned)?;
        self.append_session_entry(&session.actor, Act::SessionAbandon, id, reference)?;
        self.session(id)
    }

    fn auto_snapshot(&mut self) -> Result<(), Error> {
        if let Some(id) = self.engine.snapshot()? {
            let entry = self.entry(Act::Snapshot, Some(id))?;
            self.journal.append(&entry)?;
        }
        Ok(())
    }

    /// The gate-satisfied apply: serialize on the landing lease, then
    /// rebase and advance — or park. Editing never takes this lease; only
    /// landing does.
    fn apply(
        &mut self,
        session: &Session,
        id: RequestId,
        tip: &str,
        approver: &Actor,
    ) -> Result<GateOutcome, Error> {
        let holder = format!("{}:{}", self.actor.name, std::process::id());
        let now = now_ms()?;
        match self.coordination.claim_lease(
            LANDING_LEASE_POINT,
            &holder,
            now,
            LANDING_LEASE_TTL_MS,
        )? {
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
        let outcome = self.apply_holding_lease(session, id, tip, approver);
        let released = self
            .coordination
            .release_lease(LANDING_LEASE_POINT, &holder);
        let outcome = outcome?;
        released?;
        Ok(outcome)
    }

    fn apply_holding_lease(
        &mut self,
        session: &Session,
        id: RequestId,
        tip: &str,
        approver: &Actor,
    ) -> Result<GateOutcome, Error> {
        // Test seam: the cross-process lease test needs the winner to hold
        // the point long enough for the loser to observe `LeaseHeld`.
        if let Some(hold) = land_hold_ms()? {
            std::thread::sleep(Duration::from_millis(hold));
        }
        // Another process may have advanced the line since the gate check;
        // the lease is held, so this head stays put through the apply.
        self.engine.refresh()?;
        self.auto_snapshot()?;
        match self.engine.land(tip)? {
            LandOutcome::Conflicted => {
                self.coordination
                    .set_request_state(id.0, RequestState::Parked)?;
                self.append_session_entry(
                    approver,
                    Act::LandParked,
                    session.id,
                    Some(id.to_string()),
                )?;
                Ok(GateOutcome::Parked {
                    request: self.request(id)?,
                })
            }
            LandOutcome::Landed { snapshot } => {
                self.coordination
                    .set_request_state(id.0, RequestState::Landed)?;
                self.coordination
                    .set_session_state(session.id.0, SessionState::Landed)?;
                self.append_session_entry(
                    approver,
                    Act::Land,
                    session.id,
                    Some(format!("{id} {snapshot}")),
                )?;
                Ok(GateOutcome::Landed { snapshot })
            }
        }
    }

    /// Snapshot the session's working copy; the session's tip snapshot id.
    /// A new snapshot is journaled and runs the gate's side effects: it
    /// dismisses approvals (policy-decided) and re-opens an approved or
    /// parked request.
    fn snapshot_session(&mut self, session: &Session) -> Result<String, Error> {
        let mut engine = Engine::open(&session.working_copy, &session.actor)?;
        let new_snapshot = engine.snapshot_amend()?;
        let tip = engine.head()?;
        if let Some(new_snapshot) = new_snapshot {
            self.append_session_entry(
                &session.actor,
                Act::Snapshot,
                session.id,
                Some(new_snapshot.clone()),
            )?;
            self.gate_reacts_to_snapshot(session, &new_snapshot)?;
            // The landing engine reads this handle's view; fold the
            // session's operation in.
            self.engine.refresh()?;
        }
        Ok(tip)
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
                    // now be resolved.
                    RequestState::Approved | RequestState::Parked => self
                        .coordination
                        .set_request_state(request.id, RequestState::Open)?,
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
        Ok(Session {
            id,
            actor: Actor {
                name: row.actor_name,
                kind: row.actor_kind,
            },
            state: row.state,
            change_id,
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
    /// else leave it at the binary rung it arrived at.
    fn raised(&self, diff: Diff, sides: &DiffSides) -> Result<Diff, Error> {
        let deltas = diff
            .deltas
            .into_iter()
            .map(|delta| self.raise(delta, sides))
            .collect::<Result<Vec<Delta>, Error>>()?;
        Ok(Diff { deltas })
    }

    /// Only `Changed` deltas raise in v1: an added or removed document is
    /// already told by its listing line, without dumping its whole content.
    fn raise(&self, delta: Delta, sides: &DiffSides) -> Result<Delta, Error> {
        if delta.kind != DeltaKind::Changed {
            return Ok(delta);
        }
        let (before, after) = match self.engine.read_file_sides(sides, delta.address.as_str())? {
            (Side::Blob(before), Side::Blob(after)) => (before, after),
            (Side::TooLarge, _) | (_, Side::TooLarge) => {
                self.file_too_large(delta.address.as_str())?;
                return Ok(delta);
            }
            (Side::Absent, _) | (_, Side::Absent) => return Ok(delta),
        };
        if let Some(package) = self.detected(delta.address.as_str(), &after.bytes)? {
            let projections = (
                self.projection(package, delta.address.as_str(), &before)?,
                self.projection(package, delta.address.as_str(), &after)?,
            );
            if let (Some(before), Some(after)) = projections {
                return Ok(delta.at_text_rung(diff_lines(&before, &after), Some(package.id())));
            }
            return Ok(delta);
        }
        // "Fidelity drops to text or binary" (CONTEXT.md, Format Package):
        // a package-less document that decodes as text diffs as text —
        // content-based detection, the git model — because extension
        // allowlists would drop the source and config files agents edit
        // all day to the binary rung. Opaque bytes stay binary.
        match (as_text(&before.bytes), as_text(&after.bytes)) {
            (Some(before), Some(after)) => Ok(delta.at_text_rung(diff_lines(before, after), None)),
            _ => Ok(delta),
        }
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
}

/// Every format package built into this core, in detection order.
fn builtin_packages() -> Vec<Box<dyn FormatPackage>> {
    vec![Box::new(DocxPackage)]
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
