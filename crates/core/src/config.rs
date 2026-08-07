use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, config_err};

/// The actor a workspace attributes its snapshots and journal entries to.
#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Deserialize)]
struct ActorFile {
    actor: Option<ActorSection>,
}

#[derive(Debug, Deserialize)]
struct ActorSection {
    name: String,
    kind: String,
}

/// Resolve the actor from the first config home that applies.
///
/// Order: `$ATELIER_CONFIG_HOME/config.toml`, else
/// `$XDG_CONFIG_HOME/atelier/config.toml`, else
/// `~/.config/atelier/config.toml`. A missing file or a file without an
/// `[actor]` section yields [`Error::NoActorConfigured`].
pub fn resolve_actor() -> Result<Actor, Error> {
    let path = match actor_config_path() {
        Some(path) => path,
        None => return Err(Error::NoActorConfigured),
    };
    if !path.is_file() {
        return Err(Error::NoActorConfigured);
    }
    let text = fs::read_to_string(&path)?;
    let parsed: ActorFile = toml::from_str(&text).map_err(config_err)?;
    match parsed.actor {
        Some(section) => Ok(Actor {
            name: section.name,
            kind: section.kind,
        }),
        None => Err(Error::NoActorConfigured),
    }
}

fn actor_config_path() -> Option<PathBuf> {
    if let Ok(home) = env::var("ATELIER_CONFIG_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("atelier").join("config.toml"));
    }
    if let Ok(home) = env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("atelier")
                .join("config.toml"),
        );
    }
    None
}

/// The on-disk `.atelier/config.toml` describing one workspace.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub schema: u32,
    pub workspace: WorkspaceSection,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceSection {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    pub kind: String,
    pub path: String,
    pub sync: String,
    pub mount: String,
}

impl WorkspaceConfig {
    pub fn new(name: String) -> Self {
        Self {
            schema: 1,
            workspace: WorkspaceSection { name },
            sources: Vec::new(),
        }
    }
}

/// Read `.atelier/config.toml` from a workspace's control directory.
pub fn read_workspace_config(atelier_dir: &Path) -> Result<WorkspaceConfig, Error> {
    let text = fs::read_to_string(atelier_dir.join("config.toml"))?;
    toml::from_str(&text).map_err(config_err)
}

/// Write `.atelier/config.toml` into a workspace's control directory.
pub fn write_workspace_config(atelier_dir: &Path, config: &WorkspaceConfig) -> Result<(), Error> {
    let text = toml::to_string(config).map_err(config_err)?;
    fs::write(atelier_dir.join("config.toml"), text)?;
    Ok(())
}
