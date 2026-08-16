use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use atelier_core::{
    GateOutcome, JournalEntry, RequestId, SessionId, WatchEvent, WatchStop, Workspace, printable,
    render_diff,
};
use clap::{Parser, Subcommand};

const JOURNAL_LIMIT: usize = 100;
const HISTORY_LIMIT: usize = 100;

#[derive(Debug, Parser)]
#[command(name = "atelier", about = "Versioned workspaces for humans and agents")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize a workspace.
    Init { path: Option<PathBuf> },
    /// Attach a local folder: imported into the root, or — with --mount —
    /// as a mounted source carrying its own history.
    Attach {
        folder: PathBuf,
        /// Mount the source at this name with its own engine and history.
        #[arg(long)]
        mount: Option<String>,
    },
    /// Show what this workspace is: sources, discipline, live state, and
    /// the loop it expects. The first thing an actor reads.
    Manifest,
    /// Show the changes between the two latest snapshots.
    Diff,
    /// Show recent workspace acts.
    Journal,
    /// Show the shared lines' snapshots, newest first — every source's,
    /// or one mount's. History records content states; the journal
    /// records acts and intent.
    History { source: Option<String> },
    /// Show every session, newest first.
    Sessions,
    /// Show every landing request, newest first.
    Requests,
    /// Approve a landing request; a satisfied gate lands the change.
    Approve { request: String },
    /// Reject a landing request.
    Reject {
        request: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Land the session's change: request plus self-approval where
    /// policy allows; otherwise the request stays pending for approvers.
    Land { session: String },
    /// Watch the workspace: external edits become attributed snapshots.
    Watch {
        /// Quiet time after an edit storm before its snapshot, in milliseconds.
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
    },
    /// Serve the workspace to agents.
    Serve {
        /// Speak MCP over stdio, one client per process.
        #[arg(long)]
        mcp_stdio: bool,
        /// Speak MCP streamable HTTP at /mcp and REST under /v1.
        #[arg(long)]
        http: bool,
        /// The ip:port the HTTP server binds.
        #[arg(long, default_value = "127.0.0.1:7423")]
        bind: String,
        /// Allow a bind beyond loopback; auth arrives in a later slice,
        /// so this exposes the workspace to whoever reaches the address.
        #[arg(long)]
        allow_remote: bool,
    },
}

pub fn execute(cli: Cli) -> Result<Vec<String>> {
    match cli.command {
        Command::Init { path } => init(path),
        Command::Attach { folder, mount } => attach(&folder, mount.as_deref()),
        Command::Manifest => manifest(),
        Command::Diff => diff(),
        Command::Journal => journal(),
        Command::History { source } => history(source.as_deref()),
        Command::Sessions => sessions(),
        Command::Requests => requests(),
        Command::Approve { request } => approve(&request),
        Command::Reject { request, reason } => reject(&request, reason.as_deref()),
        Command::Land { session } => land(&session),
        Command::Watch { debounce_ms } => watch(debounce_ms),
        Command::Serve {
            mcp_stdio,
            http,
            bind,
            allow_remote,
        } => serve(mcp_stdio, http, &bind, allow_remote),
    }
}

fn init(path: Option<PathBuf>) -> Result<Vec<String>> {
    let path = match path {
        Some(path) => path,
        None => env::current_dir().context("read the current directory")?,
    };
    Workspace::init(&path)?;

    Ok(vec![format!(
        "initialized workspace {} at {}",
        workspace_name(&path),
        path.display()
    )])
}

fn attach(folder: &Path, mount: Option<&str>) -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    let mut workspace = Workspace::open(root)?;
    let source = match mount {
        Some(name) => workspace.attach_mount(folder, name)?,
        None => workspace.attach(folder)?,
    };

    let line = match mount {
        Some(name) => format!(
            "attached {} {} at {name}",
            source.kind,
            source.path.display()
        ),
        None => format!("attached {} {}", source.kind, source.path.display()),
    };
    Ok(vec![line])
}

fn manifest() -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let manifest = workspace.manifest()?;
    Ok(manifest.lines().map(str::to_owned).collect())
}

fn diff() -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let diff = workspace.diff_latest()?;

    if diff.deltas.is_empty() {
        return Ok(vec![
            "no changes between the two latest snapshots".to_owned(),
        ]);
    }

    Ok(render_diff(&diff))
}

fn journal() -> Result<Vec<String>> {
    let mut workspace = open_current()?;

    workspace
        .journal(JOURNAL_LIMIT)?
        .iter()
        .map(render_entry)
        .collect()
}

fn history(source: Option<&str>) -> Result<Vec<String>> {
    let mut workspace = open_current()?;

    let lines: Vec<String> = workspace
        .log(HISTORY_LIMIT)?
        .iter()
        .filter(|entry| match source {
            Some(source) => entry.source.as_deref() == Some(source),
            None => true,
        })
        .map(|entry| {
            let line = format!(
                "{}  {}  {}",
                entry.snapshot.id,
                entry.snapshot.actor,
                format_rfc3339_utc(entry.snapshot.at_ms)?
            );
            // A mounted source's line carries its mount when every source
            // lists; a single source's listing keeps the bare v1 shape.
            Ok(printable(&match (&entry.source, source) {
                (Some(mount), None) => format!("{mount}  {line}"),
                _ => line,
            }))
        })
        .collect::<Result<Vec<String>>>()?;
    if let (true, Some(source)) = (lines.is_empty(), source) {
        bail!("no source is mounted at {source:?}");
    }
    Ok(lines)
}

fn sessions() -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let sessions = workspace.sessions()?;

    if sessions.is_empty() {
        return Ok(vec!["no sessions".to_owned()]);
    }

    Ok(sessions
        .iter()
        .map(|session| {
            printable(&format!(
                "{}  {}  {} ({})  {}",
                session.id,
                session.state,
                session.actor.name,
                session.actor.kind,
                session.change_id
            ))
        })
        .collect())
}

fn requests() -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let requests = workspace.landing_requests()?;

    if requests.is_empty() {
        return Ok(vec!["no landing requests".to_owned()]);
    }

    Ok(requests
        .iter()
        .map(|request| {
            printable(&format!(
                "{}  {}  session {}  by {} ({})",
                request.id,
                request.state,
                request.session_id,
                request.requester.name,
                request.requester.kind
            ))
        })
        .collect())
}

fn approve(request: &str) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let id: RequestId = request.parse()?;
    let approver = workspace.actor().clone();
    let outcome = workspace.approve(id, &approver)?;
    Ok(render_outcome(&outcome))
}

fn reject(request: &str, reason: Option<&str>) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let id: RequestId = request.parse()?;
    let actor = workspace.actor().clone();
    let rejected = workspace.reject(id, &actor, reason)?;
    Ok(vec![format!("rejected {}", rejected.id)])
}

fn land(session: &str) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let id: SessionId = session.parse()?;
    let outcome = workspace.land(id)?;
    Ok(render_outcome(&outcome))
}

/// Blocks until the process is interrupted, printing each act as it
/// happens; Rust's stdout is line-buffered, so every line lands as soon
/// as it prints — a reader on the other end of a pipe sees acts live.
fn watch(debounce_ms: u64) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let root = env::current_dir().context("read the current directory")?;
    // The CLI never stops the loop itself; interrupting the process does.
    let stop = WatchStop::new();
    workspace.watch(
        Duration::from_millis(debounce_ms),
        |event| match event {
            WatchEvent::Started => println!("watching {}", root.display()),
            WatchEvent::Snapshotted { snapshot } => println!("snapshot {snapshot}"),
        },
        &stop,
    )?;
    Ok(Vec::new())
}

fn serve(mcp_stdio: bool, http: bool, bind: &str, allow_remote: bool) -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    match (mcp_stdio, http) {
        (true, false) => atelier_surface::serve_stdio(&root)?,
        (false, true) => atelier_surface::serve_http(&root, bind, allow_remote)?,
        (true, true) => {
            bail!("atelier serve speaks one transport per process: --mcp-stdio or --http")
        }
        (false, false) => bail!("atelier serve requires a transport: --mcp-stdio or --http"),
    }
    Ok(Vec::new())
}

/// The outcome as lines: one `landed …` per source (root lines keep the
/// exact v1 shape), one `parked …` per parked source.
fn render_outcome(outcome: &GateOutcome) -> Vec<String> {
    match outcome {
        GateOutcome::Landed { landings } => landings.iter().map(render_landing).collect(),
        GateOutcome::Pending { request, required } => vec![format!(
            "pending: {} of {required} approvals on {}",
            request.approvals.len(),
            request.id
        )],
        GateOutcome::Parked {
            request,
            landings,
            parked,
        } => {
            let mut lines: Vec<String> = landings.iter().map(render_landing).collect();
            for source in parked {
                let line = match source {
                    Some(name) => format!(
                        "parked {} on {name}: the change conflicts with that shared line; a new snapshot re-opens the gate",
                        request.id
                    ),
                    None => format!(
                        "parked {}: the change conflicts with the shared line; a new snapshot re-opens the gate",
                        request.id
                    ),
                };
                lines.push(line);
            }
            lines
        }
    }
}

fn render_landing(landing: &atelier_core::Landing) -> String {
    match &landing.source {
        Some(source) => format!("landed {source} {}", landing.snapshot),
        None => format!("landed {}", landing.snapshot),
    }
}

/// One journal line: time, actor, act, then whatever the act carries —
/// its session, the instruction summary in quotes with its run reference,
/// and the act's reference.
fn render_entry(entry: &JournalEntry) -> Result<String> {
    let mut parts = vec![
        format_rfc3339_utc(entry.at_ms)?,
        format!("{} ({})", entry.actor_name, entry.actor_kind),
        entry.act.to_string(),
    ];
    if let Some(session) = &entry.session {
        parts.push(session.clone());
    }
    if let Some(summary) = &entry.instruction_summary {
        parts.push(format!("\"{summary}\""));
    }
    if let Some(run_ref) = &entry.instruction_run_ref {
        parts.push(run_ref.clone());
    }
    if let Some(reference) = &entry.reference {
        parts.push(reference.clone());
    }
    Ok(printable(&parts.join("  ")))
}

fn open_current() -> Result<Workspace> {
    let root = env::current_dir().context("read the current directory")?;
    Ok(Workspace::open(root)?)
}

fn workspace_name(path: &Path) -> String {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.to_owned(),
        None => "workspace".to_owned(),
    }
}

fn format_rfc3339_utc(at_ms: i64) -> Result<String> {
    let at = time::OffsetDateTime::from_unix_timestamp(at_ms.div_euclid(1_000))
        .context("timestamp is outside the supported range")?;
    at.format(&time::format_description::well_known::Rfc3339)
        .context("format timestamp as rfc3339")
}

#[cfg(test)]
mod tests {
    use super::format_rfc3339_utc;

    #[test]
    fn formats_epoch_and_leap_day_as_utc() {
        assert_eq!(format_rfc3339_utc(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(951_782_400_000).unwrap(),
            "2000-02-29T00:00:00Z"
        );
    }
}
