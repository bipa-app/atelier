use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use atelier_core::{
    GateOutcome, JournalEntry, RequestId, SessionId, SyncOutcome, WatchEvent, WatchStop, Workspace,
    printable, render_diff,
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
    /// Attach a source: a local folder (imported into the root, or — with
    /// --mount — as a mounted source), or a bucket URL (s3://, gs://,
    /// az://), which always mounts (ADR-0012).
    Attach {
        source: String,
        /// Mount the source at this name with its own engine and history.
        #[arg(long)]
        mount: Option<String>,
    },
    /// Show what this workspace is: sources, discipline, live state, and
    /// the loop it expects. The first thing an actor reads.
    Manifest,
    /// Show the live state: per-source heads, open sessions, live requests.
    Status,
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
    /// Step a landed request back off every line it landed; the request
    /// re-opens for a new decision (ADR-0011).
    Undo { request: String },
    /// Mirror a folder source's shared line back to its origin; parks when
    /// the origin changed out-of-band (ADR-0010).
    Sync {
        /// The mounted source to sync; the root import when omitted.
        source: Option<String>,
        /// Overwrite an origin that changed out-of-band.
        #[arg(long)]
        force: bool,
    },
    /// Watch the workspace: external edits become attributed snapshots.
    Watch {
        /// Quiet time after an edit storm before its snapshot, in milliseconds.
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
    },
    /// Open a session and run a command inside its working copy: every
    /// edit the command makes is versioned, attributed, and ready to land.
    Run {
        /// One line on what this run is doing and why.
        #[arg(long)]
        summary: Option<String>,
        /// Land the session's change when the command succeeds.
        #[arg(long)]
        land: bool,
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
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
        /// Allow a bind beyond loopback; requires --token.
        #[arg(long)]
        allow_remote: bool,
        /// Require this bearer token on every HTTP request; mandatory
        /// beyond loopback.
        #[arg(long)]
        token: Option<String>,
    },
}

pub fn execute(cli: Cli) -> Result<Vec<String>> {
    match cli.command {
        Command::Init { path } => init(path),
        Command::Attach { source, mount } => attach(&source, mount.as_deref()),
        Command::Manifest => manifest(),
        Command::Status => status(),
        Command::Diff => diff(),
        Command::Journal => journal(),
        Command::History { source } => history(source.as_deref()),
        Command::Sessions => sessions(),
        Command::Requests => requests(),
        Command::Approve { request } => approve(&request),
        Command::Reject { request, reason } => reject(&request, reason.as_deref()),
        Command::Land { session } => land(&session),
        Command::Undo { request } => undo(&request),
        Command::Sync { source, force } => sync(source.as_deref(), force),
        Command::Watch { debounce_ms } => watch(debounce_ms),
        Command::Run {
            summary,
            land,
            command,
        } => run_in_session(summary.as_deref(), land, &command),
        Command::Serve {
            mcp_stdio,
            http,
            bind,
            allow_remote,
            token,
        } => serve(mcp_stdio, http, &bind, allow_remote, token.as_deref()),
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

fn attach(source: &str, mount: Option<&str>) -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    let mut workspace = Workspace::open(root)?;
    let attached = if atelier_core::is_remote_url(source) {
        let Some(name) = mount else {
            bail!("remote sources mount; pass --mount <name>");
        };
        workspace.attach_remote(source, name)?
    } else {
        match mount {
            Some(name) => workspace.attach_mount(Path::new(source), name)?,
            None => workspace.attach(Path::new(source))?,
        }
    };

    let line = match mount {
        Some(name) => format!(
            "attached {} {} at {name}",
            attached.kind,
            attached.path.display()
        ),
        None => format!("attached {} {}", attached.kind, attached.path.display()),
    };
    Ok(vec![line])
}

fn manifest() -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let manifest = workspace.manifest()?;
    Ok(manifest.lines().map(str::to_owned).collect())
}

fn status() -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let status = workspace.status()?;
    Ok(status.lines().map(str::to_owned).collect())
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

fn undo(request: &str) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let id: RequestId = request.parse()?;
    let restores = workspace.undo(id)?;
    let mut lines: Vec<String> = restores
        .iter()
        .map(|restore| match &restore.source {
            Some(source) => format!("restored {source} {}", restore.head),
            None => format!("restored {}", restore.head),
        })
        .collect();
    lines.push(format!("{id} is open again; approvals dismissed"));
    Ok(lines)
}

fn sync(source: Option<&str>, force: bool) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let outcome = workspace.sync(source, force)?;
    Ok(vec![match outcome {
        SyncOutcome::Synced { snapshot } => match source {
            Some(name) => format!("synced {name} {snapshot}"),
            None => format!("synced {snapshot}"),
        },
        SyncOutcome::Parked { .. } => {
            "parked: the origin changed since the last sync; re-run with --force to overwrite"
                .to_owned()
        }
    }])
}

fn run_in_session(
    summary: Option<&str>,
    land_after: bool,
    command: &[String],
) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let actor = workspace.actor().clone();
    let summary = match summary {
        Some(summary) => summary.to_owned(),
        None => format!("run: {}", command.join(" ")),
    };
    let instruction = atelier_core::Instruction {
        summary,
        run_ref: None,
        verbatim: None,
    };
    let session = workspace.open_session(&actor, &instruction)?;
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&session.working_copy)
        .status()
        .with_context(|| format!("run {:?}", command[0]))?;

    // The diff snapshots the session's outstanding edits either way, so a
    // failing command's work is versioned before this returns.
    let diff = workspace.session_diff(session.id)?;
    if !status.success() {
        bail!(
            "command failed ({status}); session {id} keeps the work - land with: atelier land {id}",
            id = session.id
        );
    }
    let mut lines = render_diff(&diff);
    if land_after {
        let outcome = workspace.land(session.id)?;
        lines.extend(render_outcome(&outcome));
    } else {
        lines.push(format!(
            "session {id} holds the change; land with: atelier land {id}",
            id = session.id
        ));
    }
    Ok(lines)
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

fn serve(
    mcp_stdio: bool,
    http: bool,
    bind: &str,
    allow_remote: bool,
    token: Option<&str>,
) -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    match (mcp_stdio, http) {
        (true, false) => atelier_surface::serve_stdio(&root)?,
        (false, true) => atelier_surface::serve_http(&root, bind, allow_remote, token)?,
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
