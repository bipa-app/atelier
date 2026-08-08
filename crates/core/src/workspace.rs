use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atelier_diff_core::Diff;

use crate::config::{
    Actor, Source, SourceKind, SyncPolicy, WorkspaceConfig, read_workspace_config, resolve_actor,
    write_workspace_config,
};
use crate::engine::Engine;
use crate::error::{Error, config_err};
use crate::journal::{Act, Journal, JournalEntry};

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

    /// Diff the latest snapshot against its first parent.
    pub fn diff_latest(&mut self) -> Result<Diff, Error> {
        self.auto_snapshot()?;
        self.engine.diff_latest()
    }

    /// Diff two snapshots by id: `before` against `after`.
    pub fn diff_between(&mut self, before: &str, after: &str) -> Result<Diff, Error> {
        self.auto_snapshot()?;
        self.engine.diff_between(before, after)
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
