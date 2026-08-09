use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, config_err};

/// The actor a workspace attributes its snapshots and journal entries to.
#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    pub kind: ActorKind,
}

/// What kind of actor acted: a person, an AI agent, or an automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Human,
    Agent,
    Automation,
}

impl ActorKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Automation => "automation",
        }
    }
}

impl fmt::Display for ActorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ActorKind {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            "automation" => Ok(Self::Automation),
            other => Err(Error::Config(format!("unknown actor kind: {other}"))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ActorFile {
    actor: Option<ActorSection>,
}

#[derive(Debug, Deserialize)]
struct ActorSection {
    name: String,
    kind: ActorKind,
}

/// Resolve the actor from the first config home that applies.
///
/// Order: `$ATELIER_CONFIG_HOME/config.toml`, else
/// `$XDG_CONFIG_HOME/atelier/config.toml`, else
/// `~/.config/atelier/config.toml`. A missing file or a file without an
/// `[actor]` section yields [`Error::NoActorConfigured`].
pub fn resolve_actor() -> Result<Actor, Error> {
    let Some(path) = actor_config_path() else {
        return Err(Error::NoActorConfigured);
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

/// An external origin the workspace is attached to, as held in memory and as
/// persisted under `[[source]]` in `.atelier/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub kind: SourceKind,
    pub path: PathBuf,
    pub sync: SyncPolicy,
    pub mount: PathBuf,
}

/// The kind of origin a source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    LocalFolder,
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalFolder => f.write_str("local-folder"),
        }
    }
}

/// How content moves between a workspace and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncPolicy {
    TwoWay,
}

impl fmt::Display for SyncPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TwoWay => f.write_str("two-way"),
        }
    }
}

/// The on-disk `.atelier/config.toml` describing one workspace.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub schema: u32,
    pub workspace: WorkspaceSection,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceSection {
    pub name: String,
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
