use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atelier_diff_core::{
    Delta, DeltaKind, Diff, FormatPackage, PackageId, as_text, detect_package, diff_lines,
};
use atelier_format_docx::DocxPackage;

use crate::config::{
    Actor, Source, SourceKind, SyncPolicy, WorkspaceConfig, read_workspace_config, resolve_actor,
    write_workspace_config,
};
use crate::engine::{DiffSides, Engine, FileBlob, MAX_LADDER_FILE_SIZE, Side};
use crate::error::{Error, config_err};
use crate::journal::{Act, Journal, JournalEntry};
use crate::projection::ProjectionCache;

pub use crate::engine::Snapshot;

const CONTROL_DIR: &str = ".atelier";
const JOURNAL_FILE: &str = "journal.sqlite3";
const SKIP_NAMES: [&str; 3] = [".atelier", ".jj", ".git"];

/// A named, versioned body of work content with its own history and journal.
pub struct Workspace {
    root: PathBuf,
    actor: Actor,
    engine: Engine,
    journal: Journal,
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
        let workspace = Self {
            root,
            actor,
            engine,
            journal,
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
        Ok(Self {
            root,
            actor,
            engine,
            journal,
            packages: builtin_packages(),
            projections: ProjectionCache::new(&control),
        })
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
        self.auto_snapshot()?;
        self.engine.log(limit)
    }

    /// Diff the latest snapshot against its first parent, each delta raised
    /// to the highest rung the ladder allows.
    pub fn diff_latest(&mut self) -> Result<Diff, Error> {
        self.auto_snapshot()?;
        let (diff, sides) = self.engine.diff_latest()?;
        self.raised(diff, &sides)
    }

    /// Diff two snapshots by id: `before` against `after`, each delta
    /// raised to the highest rung the ladder allows.
    pub fn diff_between(&mut self, before: &str, after: &str) -> Result<Diff, Error> {
        self.auto_snapshot()?;
        let (diff, sides) = self.engine.diff_between(before, after)?;
        self.raised(diff, &sides)
    }

    /// Read up to `limit` journal entries, newest first.
    pub fn journal(&mut self, limit: usize) -> Result<Vec<JournalEntry>, Error> {
        self.auto_snapshot()?;
        self.journal.entries(limit)
    }

    fn auto_snapshot(&mut self) -> Result<(), Error> {
        if let Some(id) = self.engine.snapshot()? {
            let entry = self.entry(Act::Snapshot, Some(id))?;
            self.journal.append(&entry)?;
        }
        Ok(())
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
            _ => return Ok(delta),
        };
        if let Some(package) = self.detected(delta.address.as_str(), &after)? {
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
    fn detected(
        &self,
        address: &str,
        blob: &FileBlob,
    ) -> Result<Option<&dyn FormatPackage>, Error> {
        match catch_unwind(AssertUnwindSafe(|| {
            detect_package(&self.packages, address, &blob.bytes)
        })) {
            Ok(package) => Ok(package),
            Err(_) => {
                self.package_failed(address, None, "a package panicked during detection")?;
                Ok(None)
            }
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
        Some(name) => name.to_string(),
        None => "workspace".to_string(),
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

fn now_ms() -> Result<i64, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(config_err)?;
    i64::try_from(elapsed.as_millis()).map_err(config_err)
}
