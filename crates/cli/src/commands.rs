use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use atelier_hosted::{HostedNode, NodeClaim, NodePaths, ReleaseOutcome, ReplicateOutcome};
use atelier_sdk::{
    Actor, ActorKind, GateOutcome, JournalEntry, PullOutcome, RequestId, SessionId, SyncOutcome,
    WatchEvent, WatchStop, Workspace, printable, render_diff,
};
use clap::{Args, Parser, Subcommand};

const JOURNAL_LIMIT: usize = 100;
const HISTORY_LIMIT: usize = 100;

#[derive(Debug, PartialEq, Eq)]
struct LocalGitPreflight {
    head: String,
    branch: Option<String>,
    tracked_modifications: usize,
    untracked_files: usize,
    untracked_bytes: u64,
}

impl LocalGitPreflight {
    fn is_dirty(&self) -> bool {
        self.tracked_modifications > 0 || self.untracked_files > 0
    }

    fn lines(&self) -> [String; 2] {
        let branch = match &self.branch {
            Some(branch) => branch.as_str(),
            None => "detached",
        };
        [
            format!("source git: HEAD {}; branch {branch}", self.head),
            format!(
                "source git state: tracked modifications: {}; untracked files: {}; estimated untracked bytes: {}",
                self.tracked_modifications, self.untracked_files, self.untracked_bytes
            ),
        ]
    }
}

#[derive(Debug, Args)]
struct ActorArgs {
    /// Attribute the session to this actor instead of the configured actor.
    #[arg(long, requires = "actor_kind")]
    actor_name: Option<String>,
    /// Attribute the session as human, agent, or automation.
    #[arg(long, requires = "actor_name")]
    actor_kind: Option<ActorKind>,
}

impl ActorArgs {
    fn resolve(self, fallback: &Actor) -> Result<Actor> {
        match (self.actor_name, self.actor_kind) {
            (Some(name), Some(kind)) if !name.is_empty() => Ok(Actor { name, kind }),
            (Some(_), Some(_)) => bail!("actor name must not be empty"),
            (None, None) => Ok(fallback.clone()),
            (Some(_), None) | (None, Some(_)) => {
                bail!("actor name and kind must be provided together")
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "atelier",
    version,
    about = "Versioned workspaces for humans and agents"
)]
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
        /// Adopt tracked and untracked changes from a dirty local Git source.
        #[arg(long)]
        allow_dirty: bool,
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
    /// Open, inspect, or abandon a long-lived session.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
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
    /// Fold bucket-side changes into a mounted remote source's line as one
    /// attributed snapshot (ADR-0012).
    Pull { source: Option<String> },
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
    /// Update this install to the latest release: runs the bundled
    /// updater the install script places beside the binary.
    Update,
    /// Open a session and run a command inside its working copy: every
    /// edit the command makes is versioned, attributed, and ready to land.
    Run {
        /// One line on what this run is doing and why.
        #[arg(long)]
        summary: Option<String>,
        /// Land the session's change when the command succeeds.
        #[arg(long)]
        land: bool,
        #[command(flatten)]
        actor: ActorArgs,
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
        /// Serve a hosted workspace (ADR-0013): claim the ownership
        /// record at this S3-compatible bucket URL, hydrate the stores,
        /// replicate while serving, release on shutdown.
        #[arg(long)]
        hosted: Option<String>,
        /// Claim as this holder; the local user and process id otherwise.
        #[arg(long)]
        holder: Option<String>,
        /// Seize the workspace from a holder that died without releasing;
        /// a plain claim refuses while the record names another node.
        #[arg(long)]
        take_over: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Open a session and print its working copy for normal file tools.
    Open {
        /// One line on what this session is doing and why.
        #[arg(long)]
        summary: String,
        #[command(flatten)]
        actor: ActorArgs,
    },
    /// Show a session's change against the shared line.
    Diff { session: String },
    /// Close a session without landing; its work stays in history.
    Abandon { session: String },
}

pub fn execute(cli: Cli) -> Result<Vec<String>> {
    match cli.command {
        Command::Init { path } => init(path),
        Command::Attach {
            source,
            mount,
            allow_dirty,
        } => attach(&source, mount.as_deref(), allow_dirty),
        Command::Manifest => manifest(),
        Command::Status => status(),
        Command::Diff => diff(),
        Command::Journal => journal(),
        Command::History { source } => history(source.as_deref()),
        Command::Sessions => sessions(),
        Command::Session { command } => match command {
            SessionCommand::Open { summary, actor } => open_session(&summary, actor),
            SessionCommand::Diff { session } => session_diff(&session),
            SessionCommand::Abandon { session } => abandon_session(&session),
        },
        Command::Requests => requests(),
        Command::Approve { request } => approve(&request),
        Command::Reject { request, reason } => reject(&request, reason.as_deref()),
        Command::Land { session } => land(&session),
        Command::Undo { request } => undo(&request),
        Command::Pull { source } => pull(source.as_deref()),
        Command::Sync { source, force } => sync(source.as_deref(), force),
        Command::Watch { debounce_ms } => watch(debounce_ms),
        Command::Update => update(),
        Command::Run {
            summary,
            land,
            actor,
            command,
        } => run_in_session(summary.as_deref(), land, actor, &command),
        Command::Serve {
            mcp_stdio,
            http,
            bind,
            allow_remote,
            token,
            hosted,
            holder,
            take_over,
        } => serve(&ServeArgs {
            mcp_stdio,
            http,
            bind: &bind,
            allow_remote,
            token: token.as_deref(),
            hosted: hosted.as_deref(),
            holder: holder.as_deref(),
            take_over,
        }),
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

/// The updater is `atelier-ws-update` (cargo-dist names it after the
/// package, and bare `atelier` is taken on crates.io); this verb is the
/// spelling people expect. It updates the tool, not a workspace, so it
/// lives on the CLI alone — no MCP or HTTP form.
fn update() -> Result<Vec<String>> {
    let current = env::current_exe().context("locate this binary")?;
    let updater = current.with_file_name("atelier-ws-update");
    if !updater.exists() {
        bail!(
            "this install has no bundled updater; re-run the install script \
             (curl -fsSL https://atelier-ws.dev/install.sh | sh) or, for cargo installs: \
             cargo install atelier-ws --force"
        );
    }
    let status = std::process::Command::new(&updater)
        .status()
        .with_context(|| format!("run {}", updater.display()))?;
    if !status.success() {
        bail!("the updater failed ({status})");
    }
    Ok(Vec::new())
}

fn attach(source: &str, mount: Option<&str>, allow_dirty: bool) -> Result<Vec<String>> {
    let source_path = Path::new(source);
    if !atelier_sdk::is_remote_url(source)
        && let Some(preflight) = local_git_preflight(source_path)?
    {
        for line in preflight.lines() {
            println!("{line}");
        }
        io::stdout().flush().context("flush source preflight")?;
        if preflight.is_dirty() && !allow_dirty {
            bail!(
                "local Git source is dirty; attach a clean clone or pass --allow-dirty to adopt these changes"
            );
        }
        if preflight.is_dirty() {
            println!("warning: --allow-dirty adopts the reported tracked and untracked changes");
            io::stdout().flush().context("flush source warning")?;
        }
    }

    let root = env::current_dir().context("read the current directory")?;
    let mut workspace = Workspace::open(root)?;
    let attached = if atelier_sdk::is_remote_url(source) {
        let Some(name) = mount else {
            bail!("remote sources mount; pass --mount <name>");
        };
        workspace.attach_remote(source, name)?
    } else {
        match mount {
            Some(name) => workspace.attach_mount(source_path, name)?,
            None => workspace.attach(source_path)?,
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

fn local_git_preflight(source: &Path) -> Result<Option<LocalGitPreflight>> {
    if !source.join(".git").is_dir() {
        return Ok(None);
    }
    let head = git_text(source, &["rev-parse", "--verify", "HEAD"])?;
    let branch = git_branch(source)?;
    let status_output = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(source)
        .output()
        .context("inspect source Git state")?;
    if !status_output.status.success() {
        bail!(
            "inspect source Git state: {}",
            String::from_utf8_lossy(&status_output.stderr).trim()
        );
    }

    let mut tracked_modifications = 0;
    let mut untracked_files = 0;
    let mut untracked_bytes = 0_u64;
    let mut skip_rename_source = false;
    for record in status_output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        if skip_rename_source {
            skip_rename_source = false;
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            bail!("inspect source Git state: malformed porcelain record");
        }
        if &record[..2] == b"??" {
            untracked_files += 1;
            let path = source.join(Path::new(OsStr::from_bytes(&record[3..])));
            let bytes = fs::symlink_metadata(&path)
                .with_context(|| format!("measure untracked source path {}", path.display()))?
                .len();
            untracked_bytes = untracked_bytes
                .checked_add(bytes)
                .context("estimated untracked source bytes overflowed u64")?;
        } else {
            tracked_modifications += 1;
            skip_rename_source =
                matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C');
        }
    }

    Ok(Some(LocalGitPreflight {
        head,
        branch,
        tracked_modifications,
        untracked_files,
        untracked_bytes,
    }))
}

fn git_branch(source: &Path) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(source)
        .output()
        .context("inspect source Git branch")?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8(output.stdout)
                .context("source Git branch is not UTF-8")?
                .trim()
                .to_owned(),
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    bail!(
        "inspect source Git branch: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn git_text(source: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(source)
        .output()
        .with_context(|| format!("inspect source Git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "inspect source Git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("source Git output is not UTF-8")?
        .trim()
        .to_owned())
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

fn open_session(summary: &str, actor: ActorArgs) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let actor = actor.resolve(workspace.actor())?;
    let instruction = atelier_sdk::Instruction {
        summary: summary.to_owned(),
        run_ref: None,
        verbatim: None,
    };
    let session = workspace.open_session(&actor, &instruction)?;
    Ok(vec![
        format!("opened session {}", session.id),
        format!("working copy {}", session.working_copy.display()),
        format!("land with: atelier land {}", session.id),
    ])
}

fn session_diff(session: &str) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let id: SessionId = session.parse()?;
    let diff = workspace.session_diff(id)?;
    if diff.deltas.is_empty() {
        return Ok(vec![format!("no changes in session {id}")]);
    }
    Ok(render_diff(&diff))
}

fn abandon_session(session: &str) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let id: SessionId = session.parse()?;
    let abandoned = workspace.abandon(id)?;
    Ok(vec![format!("abandoned {}", abandoned.id)])
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

fn pull(source: Option<&str>) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    Ok(vec![match workspace.pull(source)? {
        PullOutcome::Pulled { snapshot } => match source {
            Some(name) => format!("pulled {name} {snapshot}"),
            None => format!("pulled {snapshot}"),
        },
        PullOutcome::Current => "already current".to_owned(),
    }])
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
    actor: ActorArgs,
    command: &[String],
) -> Result<Vec<String>> {
    let mut workspace = open_current()?;
    let actor = actor.resolve(workspace.actor())?;
    let summary = match summary {
        Some(summary) => summary.to_owned(),
        None => format!("run: {}", command.join(" ")),
    };
    let instruction = atelier_sdk::Instruction {
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

/// One `atelier serve` invocation, parsed — past seven flags, and several
/// of them same-typed strings.
struct ServeArgs<'a> {
    mcp_stdio: bool,
    http: bool,
    bind: &'a str,
    allow_remote: bool,
    token: Option<&'a str>,
    hosted: Option<&'a str>,
    holder: Option<&'a str>,
    take_over: bool,
}

fn serve(args: &ServeArgs) -> Result<Vec<String>> {
    if args.hosted.is_none() && (args.holder.is_some() || args.take_over) {
        bail!("--holder and --take-over belong to --hosted");
    }
    if let Some(url) = args.hosted {
        if args.mcp_stdio || !args.http {
            bail!("a hosted workspace serves the HTTP face: --hosted needs --http");
        }
        return serve_hosted(url, args);
    }
    let root = env::current_dir().context("read the current directory")?;
    match (args.mcp_stdio, args.http) {
        (true, false) => atelier_surface::serve_stdio(&root)?,
        (false, true) => {
            atelier_surface::serve_http(&root, args.bind, args.allow_remote, args.token)?;
        }
        (true, true) => {
            bail!("atelier serve speaks one transport per process: --mcp-stdio or --http")
        }
        (false, false) => bail!("atelier serve requires a transport: --mcp-stdio or --http"),
    }
    Ok(Vec::new())
}

/// How often a hosted node replicates while serving: batched, at its own
/// pace (ADR-0013); the lag is the RPO for node loss.
const REPLICATE_EVERY: Duration = Duration::from_secs(5);

/// Claim, hydrate, serve, release (ADR-0013): the hosted half of
/// `atelier serve`. A deposed node or a failed replication stops the
/// server and surfaces on the error face — serving without a lineage
/// behind it would promise durability the bucket does not hold.
fn serve_hosted(url: &str, args: &ServeArgs) -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    let (ownership, replica) = atelier_hosted::open_planes(url)?;
    let paths = NodePaths {
        store: root.join(".atelier").join("journal.sqlite3"),
        root: root.clone(),
        replica,
    };
    let holder = match args.holder {
        Some(name) => name.to_owned(),
        None => default_holder(),
    };
    let claim = if args.take_over {
        HostedNode::take_over(ownership, &holder, &paths)?
    } else {
        HostedNode::claim(ownership, &holder, &paths)?
    };
    let mut node = match claim {
        NodeClaim::Serving(node) => *node,
        NodeClaim::HeldByOther { holder, epoch } => bail!(
            "the workspace is held by {holder} at epoch {epoch}; --take-over seizes it from a dead node"
        ),
    };
    // Working copies rematerialize before the face opens; a workspace
    // already whole on this machine opens as-is.
    Workspace::rematerialize(&root)?;
    println!("serving as {holder} at epoch {}", node.epoch());

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
        .context("install the shutdown handler")?;
    let mut last = Instant::now();
    let mut refusal = None;
    atelier_surface::serve_http_until(&root, args.bind, args.allow_remote, args.token, || {
        if stop.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if last.elapsed() < REPLICATE_EVERY {
            return Ok(true);
        }
        match node.replicate() {
            Ok(ReplicateOutcome::Acknowledged) => {
                last = Instant::now();
                Ok(true)
            }
            Ok(ReplicateOutcome::Deposed) => {
                refusal = Some(
                    "deposed: the record names another node; this node's writes land in a superseded lineage"
                        .to_owned(),
                );
                Ok(false)
            }
            Err(error) => {
                refusal = Some(format!("replication failed: {error}"));
                Ok(false)
            }
        }
    })?;
    if let Some(reason) = refusal {
        bail!(reason);
    }
    match node.release()? {
        ReleaseOutcome::Released => Ok(vec!["released".to_owned()]),
        ReleaseOutcome::NotHeld => Ok(vec![
            "the workspace moved on; nothing to release".to_owned(),
        ]),
    }
}

/// This node's identity on the record when `--holder` is absent: the
/// local user and process id — distinct per run; the epoch keeps even a
/// colliding name from sharing a lineage.
fn default_holder() -> String {
    // No USER in the environment is a bare context, not a failure; the
    // pid still distinguishes the holder.
    let user = env::var("USER").unwrap_or_else(|_| "node".to_owned());
    format!("{user}-{}", std::process::id())
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

fn render_landing(landing: &atelier_sdk::Landing) -> String {
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
