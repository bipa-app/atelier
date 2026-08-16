use std::io::{BufRead, Write};
use std::path::Path;

use atelier_core::{
    Actor, ActorKind, Error, GateOutcome, Instruction, JournalEntry, LandingRequest, RequestId,
    SessionId, Workspace, render_diff,
};
use serde_json::{Value, json};

/// The revision of the MCP specification this server speaks.
const PROTOCOL_VERSION: &str = "2025-03-26";

const JOURNAL_LIMIT: usize = 100;

/// Serve the workspace at `root` to one MCP client over stdio: one
/// JSON-RPC message per line, requests answered in order, notifications
/// consumed silently.
pub fn serve_stdio(root: &Path) -> Result<(), Error> {
    let mut workspace = Workspace::open(root)?;
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_message(&mut workspace, &line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub(crate) fn handle_message(workspace: &mut Workspace, line: &str) -> Option<String> {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return Some(error_response(&Value::Null, -32700, "parse error"));
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // A message without an id is a notification: consumed, never answered.
    let id = message.get("id").cloned()?;
    let response = match method.as_str() {
        "initialize" => result_response(
            &id,
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "atelier", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "ping" => result_response(&id, &json!({})),
        "tools/list" => result_response(&id, &json!({"tools": tool_definitions()})),
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            tools_call(workspace, &id, &params)
        }
        other => error_response(&id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

fn tools_call(workspace: &mut Workspace, id: &Value, params: &Value) -> String {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error_response(id, -32602, "tools/call needs a tool name");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match dispatch(workspace, name, &arguments) {
        Ok(value) => result_response(
            id,
            &json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "isError": false,
            }),
        ),
        Err(ToolFailure::UnknownTool) => {
            error_response(id, -32602, &format!("unknown tool: {name}"))
        }
        Err(ToolFailure::BadArguments(message)) => error_response(id, -32602, &message),
        Err(ToolFailure::Domain(error)) => result_response(
            id,
            &json!({
                "content": [{"type": "text", "text": format!("error: {error}")}],
                "isError": true,
            }),
        ),
    }
}

/// Why a tool call produced no result: the caller broke the protocol, or
/// the workspace refused — refusals travel back as tool errors the agent
/// can act on.
pub(crate) enum ToolFailure {
    UnknownTool,
    BadArguments(String),
    Domain(Error),
}

impl From<Error> for ToolFailure {
    fn from(error: Error) -> Self {
        Self::Domain(error)
    }
}

pub(crate) fn dispatch(
    workspace: &mut Workspace,
    tool: &str,
    args: &Value,
) -> Result<Value, ToolFailure> {
    match tool {
        "manifest" => Ok(json!({"manifest": workspace.manifest()?})),
        "status" => Ok(json!({"status": workspace.status()?})),
        "open_session" => {
            let session = workspace.open_session(&named_actor(args)?, &instruction_args(args)?)?;
            Ok(json!({
                "session_id": session.id.to_string(),
                "working_copy": session.working_copy.display().to_string(),
                "change_id": session.change_id,
            }))
        }
        "read" => {
            let id = session_id(args)?;
            let path = required_str(args, "path")?;
            let start = optional_usize(args, "start")?.unwrap_or_default();
            let result =
                workspace.session_read(id, path, start, optional_usize(args, "max_bytes")?)?;
            Ok(read_json(&result))
        }
        "write" => {
            let id = session_id(args)?;
            let path = required_str(args, "path")?;
            let content = required_str(args, "content")?;
            let snapshot = workspace.session_write(id, path, content)?;
            Ok(json!({"snapshot_id": snapshot}))
        }
        "diff" => {
            let diff = workspace.session_diff(session_id(args)?)?;
            Ok(json!({"diff": render_diff(&diff).join("\n")}))
        }
        "request_land" => {
            let request = workspace.request_land(session_id(args)?)?;
            Ok(request_json(&request))
        }
        "approve" => {
            let id = request_id(args)?;
            let approver = calling_actor(workspace, args)?;
            Ok(outcome_json(&workspace.approve(id, &approver)?))
        }
        "reject" => {
            let id = request_id(args)?;
            let actor = calling_actor(workspace, args)?;
            let request = workspace.reject(id, &actor, optional_str(args, "reason")?)?;
            Ok(request_json(&request))
        }
        "land" => Ok(outcome_json(&workspace.land(session_id(args)?)?)),
        "undo" => {
            let id = request_id(args)?;
            Ok(undo_json(id, &workspace.undo(id)?))
        }
        "landing_requests" => Ok(requests_json(workspace)?),
        "journal" => {
            let limit = optional_usize(args, "limit")?.unwrap_or(JOURNAL_LIMIT);
            let entries: Vec<Value> = workspace.journal(limit)?.iter().map(entry_json).collect();
            Ok(json!({"entries": entries}))
        }
        "abandon" => Ok(session_state_json(&workspace.abandon(session_id(args)?)?)),
        "sync" => {
            let force = args["force"].as_bool().unwrap_or(false);
            Ok(sync_json(
                &workspace.sync(optional_str(args, "source")?, force)?,
            ))
        }
        "pull" => Ok(pull_json(&workspace.pull(optional_str(args, "source")?)?)),
        _ => Err(ToolFailure::UnknownTool),
    }
}

/// The typed actor a call names on the wire — the parse boundary for
/// `actor_name` and `actor_kind` (AGENTS.md: parse, don't validate).
fn named_actor(args: &Value) -> Result<Actor, ToolFailure> {
    Ok(Actor {
        name: required_str(args, "actor_name")?.to_owned(),
        kind: actor_kind(required_str(args, "actor_kind")?)?,
    })
}

/// The typed instruction a call carries — summary required, run
/// reference and verbatim body optional (ADR-0004 decides what persists).
fn instruction_args(args: &Value) -> Result<Instruction, ToolFailure> {
    Ok(Instruction {
        summary: required_str(args, "instruction_summary")?.to_owned(),
        run_ref: optional_str(args, "instruction_run_ref")?.map(str::to_owned),
        verbatim: optional_str(args, "instruction_verbatim")?.map(str::to_owned),
    })
}

/// Every landing request as the wire carries them, newest first.
fn requests_json(workspace: &mut Workspace) -> Result<Value, ToolFailure> {
    let requests: Vec<Value> = workspace
        .landing_requests()?
        .iter()
        .map(request_json)
        .collect();
    Ok(json!({"requests": requests}))
}

/// A sync outcome as the wire carries it (ADR-0010).
fn sync_json(outcome: &atelier_core::SyncOutcome) -> Value {
    match outcome {
        atelier_core::SyncOutcome::Synced { snapshot } => {
            json!({"state": "synced", "snapshot_id": snapshot})
        }
        atelier_core::SyncOutcome::Parked { snapshot } => {
            json!({"state": "parked", "snapshot_id": snapshot})
        }
    }
}

/// A pull outcome as the wire carries it (ADR-0012).
fn pull_json(outcome: &atelier_core::PullOutcome) -> Value {
    match outcome {
        atelier_core::PullOutcome::Pulled { snapshot } => {
            json!({"state": "pulled", "snapshot_id": snapshot})
        }
        atelier_core::PullOutcome::Current => json!({"state": "current"}),
    }
}

/// A session's identity and state as the wire carries them.
fn session_state_json(session: &atelier_core::Session) -> Value {
    json!({
        "session_id": session.id.to_string(),
        "state": session.state.as_str(),
    })
}

/// An undo as the wire carries it: the re-opened request and each line's
/// restored head, `null` source naming the root.
fn undo_json(id: atelier_core::RequestId, restores: &[atelier_core::Restore]) -> Value {
    let lines: Vec<Value> = restores
        .iter()
        .map(|restore| json!({"source": restore.source, "head": restore.head}))
        .collect();
    json!({"request_id": id.to_string(), "state": "open", "restored": lines})
}

/// A windowed read as the wire carries it: content, window, continuation.
fn read_json(result: &atelier_core::ReadResult) -> Value {
    json!({
        "content": result.content,
        "window": {
            "start": result.window.start,
            "end": result.window.end,
            "total": result.window.total,
        },
        "next": result.next,
        "projected_by": result.projected_by.map(|package| package.to_string()),
    })
}

/// The actor a call acts as: the named one when the call carries an
/// identity, else the actor this server is configured as.
fn calling_actor(workspace: &Workspace, args: &Value) -> Result<Actor, ToolFailure> {
    match optional_str(args, "actor_name")? {
        Some(_) => named_actor(args),
        None => Ok(workspace.actor().clone()),
    }
}

fn session_id(args: &Value) -> Result<SessionId, ToolFailure> {
    Ok(required_str(args, "session_id")?.parse::<SessionId>()?)
}

fn request_id(args: &Value) -> Result<RequestId, ToolFailure> {
    Ok(required_str(args, "request_id")?.parse::<RequestId>()?)
}

fn actor_kind(text: &str) -> Result<ActorKind, ToolFailure> {
    text.parse::<ActorKind>()
        .map_err(|error| ToolFailure::BadArguments(error.to_string()))
}

fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolFailure> {
    match args.get(name) {
        Some(value) => value
            .as_str()
            .ok_or_else(|| ToolFailure::BadArguments(format!("{name} must be a string"))),
        None => Err(ToolFailure::BadArguments(format!("{name} is required"))),
    }
}

fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolFailure> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| ToolFailure::BadArguments(format!("{name} must be a string"))),
    }
}

fn optional_usize(args: &Value, name: &str) -> Result<Option<usize>, ToolFailure> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) => Ok(Some(number as usize)),
            None => Err(ToolFailure::BadArguments(format!(
                "{name} must be a non-negative integer"
            ))),
        },
    }
}

fn request_json(request: &LandingRequest) -> Value {
    let approvals: Vec<Value> = request
        .approvals
        .iter()
        .map(|approval| {
            json!({
                "actor_name": approval.actor.name,
                "actor_kind": approval.actor.kind.as_str(),
                "snapshot_id": approval.snapshot,
            })
        })
        .collect();
    json!({
        "request_id": request.id.to_string(),
        "session_id": request.session_id.to_string(),
        "state": request.state.as_str(),
        "requester_name": request.requester.name,
        "requester_kind": request.requester.kind.as_str(),
        "approvals": approvals,
    })
}

fn outcome_json(outcome: &GateOutcome) -> Value {
    match outcome {
        GateOutcome::Landed { landings } => json!({
            "state": "landed",
            "landings": landings_json(landings),
        }),
        GateOutcome::Pending { request, required } => json!({
            "state": "pending",
            "request_id": request.id.to_string(),
            "approvals": request.approvals.len(),
            "required": required,
        }),
        GateOutcome::Parked {
            request,
            landings,
            parked,
        } => json!({
            "state": "parked",
            "request_id": request.id.to_string(),
            "landings": landings_json(landings),
            "parked": parked.iter().map(|source| source_json(source.as_deref())).collect::<Vec<Value>>(),
        }),
    }
}

fn landings_json(landings: &[atelier_core::Landing]) -> Vec<Value> {
    landings
        .iter()
        .map(|landing| {
            json!({
                "source": source_json(landing.source.as_deref()),
                "snapshot_id": landing.snapshot,
            })
        })
        .collect()
}

/// A source over the wire: its mount name, or `null` for the root.
fn source_json(source: Option<&str>) -> Value {
    match source {
        Some(name) => Value::String(name.to_owned()),
        None => Value::Null,
    }
}

fn entry_json(entry: &JournalEntry) -> Value {
    json!({
        "at_ms": entry.at_ms,
        "actor_name": entry.actor_name,
        "actor_kind": entry.actor_kind.as_str(),
        "act": entry.act.as_str(),
        "session": entry.session,
        "instruction_summary": entry.instruction_summary,
        "instruction_run_ref": entry.instruction_run_ref,
        "reference": entry.reference,
    })
}

fn result_response(id: &Value, result: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// The tool surface, spoken in the glossary: session, change, land,
/// journal — never the engine's vocabulary.
/// The tool surface, spoken in the glossary: session, change, land,
/// journal — never the engine's vocabulary. Ordered for the arriving
/// agent: read models first, then the session verbs, then the gate's.
fn tool_definitions() -> Value {
    let mut tools = read_model_tools();
    tools.extend(session_tools());
    tools.extend(gate_tools());
    tools.extend(source_tools());
    Value::Array(tools)
}

/// The source verbs: how content moves between a line and its origin.
fn source_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "sync",
            "description": "Mirror a folder or remote source's shared line back to its origin; parks when the origin changed out-of-band (force overwrites deliberately, ADR-0010/0012).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "The mounted source; the root import when omitted"},
                    "force": {"type": "boolean"}
                }
            }
        }),
        json!({
            "name": "pull",
            "description": "Fold bucket-side changes into a mounted remote source's line as one attributed snapshot; refuses when the line moved locally since its last sync (ADR-0012).",
            "inputSchema": {
                "type": "object",
                "properties": {"source": {"type": "string"}}
            }
        }),
    ]
}

/// The read models: what an actor consults before and while it works.
fn read_model_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "manifest",
            "description": "Read this first: what this workspace is - its sources and mounts, its landing discipline, its live state, and the loop it expects you to follow.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "status",
            "description": "The live state: per-source heads, open sessions, live requests.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "read",
            "description": "Read a file in the session's working copy, windowed. Documents with a format package read as their text projection; the response carries a continuation offset when more remains.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "path": {"type": "string"},
                    "start": {"type": "integer", "description": "Byte offset to start from; use the last response's next"},
                    "max_bytes": {"type": "integer", "description": "Window size, at most 50000"}
                },
                "required": ["session_id", "path"]
            }
        }),
        json!({
            "name": "diff",
            "description": "The session's change against the shared-line snapshot it forked from, rendered at the highest fidelity available.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "landing_requests",
            "description": "Every landing request, newest first, with its gate state and approvals.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "journal",
            "description": "The workspace's record of acts: who did what, in which session, on whose instruction.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer"}}
            }
        }),
    ]
}

/// The session verbs: a working copy and a change of one's own.
fn session_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "open_session",
            "description": "Open a session: your own working copy of the workspace and your own change. Edit files under working_copy (or with write), then diff and request_land.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "actor_name": {"type": "string"},
                    "actor_kind": {"type": "string", "enum": ["human", "agent", "automation"]},
                    "instruction_summary": {"type": "string", "description": "One line on what this session is doing and why"},
                    "instruction_run_ref": {"type": "string", "description": "Reference to the run that issued the instruction"},
                    "instruction_verbatim": {"type": "string", "description": "The instruction verbatim; kept only under a verbatim-capture policy"}
                },
                "required": ["actor_name", "actor_kind", "instruction_summary"]
            }
        }),
        json!({
            "name": "write",
            "description": "Write a file in the session's working copy and snapshot the change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["session_id", "path", "content"]
            }
        }),
        json!({
            "name": "land",
            "description": "Land the session's change: request plus self-approval where policy allows.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "abandon",
            "description": "Close the session without landing; its work stays in history.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        }),
    ]
}

/// The gate verbs: how a change asks to land and who lets it — and how
/// a landing steps back.
fn gate_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "undo",
            "description": "Step a landed request back off every line it landed; the request re-opens with approvals dismissed and the session holds its change again (ADR-0011).",
            "inputSchema": {
                "type": "object",
                "properties": {"request_id": {"type": "string"}},
                "required": ["request_id"]
            }
        }),
        json!({
            "name": "request_land",
            "description": "Open the session's landing request. The change lands once the request's gate is satisfied; landing is never a direct write.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "approve",
            "description": "Approve a landing request. When the gate is satisfied the change lands (or the request parks on a conflict).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": {"type": "string"},
                    "actor_name": {"type": "string", "description": "Approve as this actor; the server's configured actor otherwise"},
                    "actor_kind": {"type": "string", "enum": ["human", "agent", "automation"]}
                },
                "required": ["request_id"]
            }
        }),
        json!({
            "name": "reject",
            "description": "Reject a landing request; the session stays open.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": {"type": "string"},
                    "reason": {"type": "string"},
                    "actor_name": {"type": "string"},
                    "actor_kind": {"type": "string", "enum": ["human", "agent", "automation"]}
                },
                "required": ["request_id"]
            }
        }),
    ]
}
