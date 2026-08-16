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

fn handle_message(workspace: &mut Workspace, line: &str) -> Option<String> {
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
enum ToolFailure {
    UnknownTool,
    BadArguments(String),
    Domain(Error),
}

impl From<Error> for ToolFailure {
    fn from(error: Error) -> Self {
        Self::Domain(error)
    }
}

fn dispatch(workspace: &mut Workspace, tool: &str, args: &Value) -> Result<Value, ToolFailure> {
    match tool {
        "open_session" => {
            let actor = Actor {
                name: required_str(args, "actor_name")?.to_owned(),
                kind: actor_kind(required_str(args, "actor_kind")?)?,
            };
            let instruction = Instruction {
                summary: required_str(args, "instruction_summary")?.to_owned(),
                run_ref: optional_str(args, "instruction_run_ref")?.map(str::to_owned),
                verbatim: optional_str(args, "instruction_verbatim")?.map(str::to_owned),
            };
            let session = workspace.open_session(&actor, &instruction)?;
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
            Ok(json!({
                "content": result.content,
                "window": {
                    "start": result.window.start,
                    "end": result.window.end,
                    "total": result.window.total,
                },
                "next": result.next,
                "projected_by": result.projected_by.map(|package| package.to_string()),
            }))
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
        "landing_requests" => {
            let requests: Vec<Value> = workspace
                .landing_requests()?
                .iter()
                .map(request_json)
                .collect();
            Ok(json!({"requests": requests}))
        }
        "journal" => {
            let limit = match optional_usize(args, "limit")? {
                Some(limit) => limit,
                None => JOURNAL_LIMIT,
            };
            let entries: Vec<Value> = workspace.journal(limit)?.iter().map(entry_json).collect();
            Ok(json!({"entries": entries}))
        }
        "abandon" => {
            let session = workspace.abandon(session_id(args)?)?;
            Ok(json!({
                "session_id": session.id.to_string(),
                "state": session.state.as_str(),
            }))
        }
        _ => Err(ToolFailure::UnknownTool),
    }
}

/// The actor a call acts as: the named one when the call carries an
/// identity, else the actor this server is configured as.
fn calling_actor(workspace: &Workspace, args: &Value) -> Result<Actor, ToolFailure> {
    match optional_str(args, "actor_name")? {
        Some(name) => Ok(Actor {
            name: name.to_owned(),
            kind: actor_kind(required_str(args, "actor_kind")?)?,
        }),
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
        GateOutcome::Landed { snapshot } => json!({
            "state": "landed",
            "snapshot_id": snapshot,
        }),
        GateOutcome::Pending { request, required } => json!({
            "state": "pending",
            "request_id": request.id.to_string(),
            "approvals": request.approvals.len(),
            "required": required,
        }),
        GateOutcome::Parked { request } => json!({
            "state": "parked",
            "request_id": request.id.to_string(),
        }),
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
fn tool_definitions() -> Value {
    json!([
        {
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
        },
        {
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
        },
        {
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
        },
        {
            "name": "diff",
            "description": "The session's change against the shared-line snapshot it forked from, rendered at the highest fidelity available.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        },
        {
            "name": "request_land",
            "description": "Open the session's landing request. The change lands once the request's gate is satisfied; landing is never a direct write.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        },
        {
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
        },
        {
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
        },
        {
            "name": "land",
            "description": "Land the session's change: request plus self-approval where policy allows.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        },
        {
            "name": "landing_requests",
            "description": "Every landing request, newest first, with its gate state and approvals.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "journal",
            "description": "The workspace's record of acts: who did what, in which session, on whose instruction.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer"}}
            }
        },
        {
            "name": "abandon",
            "description": "Close the session without landing; its work stays in history.",
            "inputSchema": {
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }
        }
    ])
}
