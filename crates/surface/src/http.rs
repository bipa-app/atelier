use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use atelier_sdk::{Error, Workspace, render_diff};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::mcp::{ToolFailure, dispatch, handle_message};

/// The most an HTTP request body may carry. Requests hold file content and
/// JSON-RPC messages; anything past this is not a workspace operation.
const BODY_SIZE_MAX: usize = 8 * 1024 * 1024;
// A full read window always fits in one response body.
const _: () = assert!(atelier_sdk::READ_WINDOW_MAX <= BODY_SIZE_MAX);

/// Serve the workspace at `root` over HTTP: MCP streamable HTTP at
/// `/mcp`, the same verbs as plain REST under `/v1` — one dispatch,
/// transports add reach, never capability (ADR-0006). Binding anywhere
/// but a loopback address needs `allow_remote`: auth is a dedicated slice
/// before any remote exposure.
///
/// Blocks forever, one request at a time — every operation works on the
/// one workspace, and the landing lease already serializes applies across
/// processes. Prints the resolved address once listening, so a caller
/// that bound port 0 learns where the server landed.
pub fn serve_http(
    root: &Path,
    bind: &str,
    allow_remote: bool,
    token: Option<&str>,
) -> Result<(), Error> {
    serve_http_until(root, bind, allow_remote, token, || Ok(true))
}

/// [`serve_http`] with a pulse: `tick` runs between requests, at least
/// about once a second. `Ok(true)` keeps serving, `Ok(false)` stops the
/// server cleanly, an error stops it with the error — the hosted face
/// replicates and watches its shutdown flag there (ADR-0013).
pub fn serve_http_until(
    root: &Path,
    bind: &str,
    allow_remote: bool,
    token: Option<&str>,
    mut tick: impl FnMut() -> Result<bool, Error>,
) -> Result<(), Error> {
    let address: SocketAddr = bind
        .parse()
        .map_err(|_| Error::Config(format!("bind address {bind:?} is not ip:port")))?;
    if !address.ip().is_loopback() && !allow_remote {
        return Err(Error::Config(format!(
            "binding {address} exposes the workspace beyond this machine; pass --allow-remote to mean it"
        )));
    }
    if !address.ip().is_loopback() && token.is_none() {
        return Err(Error::Config(format!(
            "binding {address} beyond loopback requires --token; every request must carry it"
        )));
    }
    let mut workspace = Workspace::open(root)?;
    let server = Server::http(address)
        .map_err(|error| Error::Config(format!("cannot bind {address}: {error}")))?;
    let listening = server
        .server_addr()
        .to_ip()
        .map_or_else(|| address.to_string(), |addr| addr.to_string());
    println!("listening on http://{listening}");
    let content_types = ContentTypes::new()?;

    loop {
        if !tick()? {
            return Ok(());
        }
        let Some(mut request) = server
            .recv_timeout(Duration::from_secs(1))
            .map_err(Error::Io)?
        else {
            continue;
        };
        let reply = if authorized(&request, token) {
            reply(&mut workspace, &mut request, &content_types)
        } else {
            Response::from_string(
                json!({"error": "unauthorized: send Authorization: Bearer <token>"}).to_string(),
            )
            .with_status_code(401)
            .with_header(content_types.json.clone())
        };
        // A client gone before its response is its loss, never the
        // server's: the loop serves the next request.
        let _ = request.respond(reply);
    }
}

/// The response headers, built once — `Header` parsing can only fail on
/// malformed literals, and failing at startup beats panicking per request.
struct ContentTypes {
    json: Header,
    text: Header,
}

impl ContentTypes {
    fn new() -> Result<Self, Error> {
        Ok(Self {
            json: header("application/json")?,
            text: header("text/plain; charset=utf-8")?,
        })
    }
}

fn header(content_type: &str) -> Result<Header, Error> {
    Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .map_err(|()| Error::Config(format!("content type {content_type:?} is not a header")))
}

fn reply(
    workspace: &mut Workspace,
    request: &mut Request,
    content_types: &ContentTypes,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let method = request.method().clone();
    let url = request.url().to_owned();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url.as_str(), None),
    };
    let body = match body(request) {
        Ok(body) => body,
        Err(reply) => return reply.with_header(content_types.json.clone()),
    };
    match route(workspace, &method, path, query, &body) {
        Route::Json { status, body } => Response::from_string(body.to_string())
            .with_status_code(status)
            .with_header(content_types.json.clone()),
        Route::Text { status, body } => Response::from_string(body)
            .with_status_code(status)
            .with_header(content_types.text.clone()),
        Route::Accepted => Response::from_string(String::new()).with_status_code(202),
    }
}

/// Whether the request may speak to this workspace: without a configured
/// token the server is an open loopback face; with one, every request
/// must carry it as a bearer.
fn authorized(request: &Request, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    let provided = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("authorization"))
        .map(|header| header.value.as_str());
    let Some(provided) = provided else {
        return false;
    };
    let Some(provided) = provided.strip_prefix("Bearer ") else {
        return false;
    };
    token_matches(provided.as_bytes(), token.as_bytes())
}

/// Constant-time comparison: the reply must not say how much of the
/// token matched. Iterates the expected length, leaking only that.
fn token_matches(provided: &[u8], expected: &[u8]) -> bool {
    let mut difference = u8::from(provided.len() != expected.len());
    for (index, byte) in expected.iter().enumerate() {
        difference |= byte ^ provided.get(index).copied().unwrap_or(0);
    }
    difference == 0
}

fn body(request: &mut Request) -> Result<String, Response<std::io::Cursor<Vec<u8>>>> {
    let mut body = String::new();
    let mut reader = request.as_reader().take(BODY_SIZE_MAX as u64 + 1);
    if reader.read_to_string(&mut body).is_err() {
        return Err(Response::from_string(
            json!({"error": "the request body is not utf-8"}).to_string(),
        )
        .with_status_code(400));
    }
    if body.len() > BODY_SIZE_MAX {
        return Err(Response::from_string(
            json!({"error": format!("the request body exceeds {BODY_SIZE_MAX} bytes")}).to_string(),
        )
        .with_status_code(413));
    }
    Ok(body)
}

/// What one routed request produced.
enum Route {
    Json {
        status: u16,
        body: Value,
    },
    Text {
        status: u16,
        body: String,
    },
    /// A JSON-RPC notification: consumed, never answered (202).
    Accepted,
}

fn route(
    workspace: &mut Workspace,
    method: &Method,
    path: &str,
    query: Option<&str>,
    body: &str,
) -> Route {
    match (method, path) {
        // MCP streamable HTTP: one JSON-RPC message per POST, answered as
        // JSON. This server opens no server-initiated streams, which the
        // GET arm below refuses per the specification.
        (Method::Post, "/mcp") => match handle_message(workspace, body) {
            Some(response) => match serde_json::from_str(&response) {
                Ok(message) => Route::Json {
                    status: 200,
                    body: message,
                },
                Err(error) => error_route(500, &format!("response was not json: {error}")),
            },
            None => Route::Accepted,
        },
        (Method::Get, "/mcp") => error_route(405, "this server opens no server-initiated streams"),
        (Method::Get, "/v1/diff") => match workspace.diff_latest() {
            // The exact lines `atelier diff` prints for the same snapshots:
            // both faces render through render_diff (ADR-0006).
            Ok(diff) => {
                let mut body = render_diff(&diff).join("\n");
                if !body.is_empty() {
                    body.push('\n');
                }
                Route::Text { status: 200, body }
            }
            Err(error) => error_route(500, &error.to_string()),
        },
        (Method::Get, "/v1/manifest") => text_model(workspace.manifest()),
        (Method::Get, "/v1/status") => text_model(workspace.status()),
        (Method::Get, "/v1/requests") => json_call(workspace, "landing_requests", &json!({})),
        (Method::Post, "/v1/sessions") => json_call(workspace, "open_session", &parse_args(body)),
        (Method::Get, "/v1/journal") => {
            let mut args = json!({});
            if let Some(limit) = query_value(query, "limit") {
                match limit.parse::<u64>() {
                    Ok(limit) => args["limit"] = json!(limit),
                    Err(_) => return error_route(400, "limit must be a number"),
                }
            }
            json_call(workspace, "journal", &args)
        }
        _ => match path.strip_prefix("/v1/requests/") {
            Some(rest) => request_route(workspace, method, rest, body),
            None => match path.strip_prefix("/v1/sources/") {
                Some(rest) => source_route(workspace, method, rest, body),
                None => session_route(workspace, method, path, query, body),
            },
        },
    }
}

/// A read model as text/plain: the exact lines the CLI prints for the
/// same state — every face renders through the one core (ADR-0006).
fn text_model(model: Result<String, Error>) -> Route {
    match model {
        Ok(mut body) => {
            if !body.is_empty() {
                body.push('\n');
            }
            Route::Text { status: 200, body }
        }
        Err(error) => error_route(500, &error.to_string()),
    }
}

/// The `/v1/sources/{mount}/…` routes: how content moves between a line
/// and its origin. The body may carry `force` for a sync.
fn source_route(workspace: &mut Workspace, method: &Method, rest: &str, body: &str) -> Route {
    let Some((mount, action)) = rest.split_once('/') else {
        return error_route(404, "no such resource");
    };
    let mut args = match parse_args(body) {
        Value::Object(map) => Value::Object(map),
        Value::Null => json!({}),
        _ => return error_route(400, "the body must be a json object"),
    };
    args["source"] = json!(mount);
    match (method, action) {
        (Method::Post, "sync") => json_call(workspace, "sync", &args),
        (Method::Post, "pull") => json_call(workspace, "pull", &args),
        _ => error_route(404, "no such resource"),
    }
}

/// The `/v1/requests/{id}/…` routes: the gate's verbs. The body may name
/// the acting actor (`actor_name`, `actor_kind`) and a rejection `reason`.
fn request_route(workspace: &mut Workspace, method: &Method, rest: &str, body: &str) -> Route {
    let Some((id, action)) = rest.split_once('/') else {
        return error_route(404, "no such resource");
    };
    let mut args = match parse_args(body) {
        Value::Object(map) => Value::Object(map),
        Value::Null => json!({}),
        _ => return error_route(400, "the body must be a json object"),
    };
    args["request_id"] = json!(id);
    match (method, action) {
        (Method::Post, "approve") => json_call(workspace, "approve", &args),
        (Method::Post, "reject") => json_call(workspace, "reject", &args),
        (Method::Post, "undo") => json_call(workspace, "undo", &args),
        _ => error_route(404, "no such resource"),
    }
}

/// The `/v1/sessions/{id}/…` routes: the same verbs the MCP tools speak.
fn session_route(
    workspace: &mut Workspace,
    method: &Method,
    path: &str,
    query: Option<&str>,
    body: &str,
) -> Route {
    let Some(rest) = path.strip_prefix("/v1/sessions/") else {
        return error_route(404, "no such resource");
    };
    let Some((id, action)) = rest.split_once('/') else {
        return error_route(404, "no such resource");
    };
    match (method, action) {
        (Method::Get, "diff") => json_call(workspace, "diff", &json!({"session_id": id})),
        (Method::Post, "land") => json_call(workspace, "land", &json!({"session_id": id})),
        (Method::Post, "request-land") => {
            json_call(workspace, "request_land", &json!({"session_id": id}))
        }
        (Method::Post, "abandon") => json_call(workspace, "abandon", &json!({"session_id": id})),
        (Method::Get, file) if file.starts_with("files/") => {
            let file_path = &file["files/".len()..];
            if file_path.is_empty() {
                return error_route(404, "no such resource");
            }
            let mut args = json!({"session_id": id, "path": file_path});
            for name in ["start", "max_bytes"] {
                if let Some(value) = query_value(query, name) {
                    match value.parse::<u64>() {
                        Ok(value) => args[name] = json!(value),
                        Err(_) => return error_route(400, "start and max_bytes must be numbers"),
                    }
                }
            }
            json_call(workspace, "read", &args)
        }
        (Method::Put, file) if file.starts_with("files/") => {
            let file_path = &file["files/".len()..];
            if file_path.is_empty() {
                return error_route(404, "no such resource");
            }
            json_call(
                workspace,
                "write",
                &json!({"session_id": id, "path": file_path, "content": body}),
            )
        }
        _ => error_route(404, "no such resource"),
    }
}

/// One REST call through the one dispatch every transport shares.
fn json_call(workspace: &mut Workspace, tool: &str, args: &Value) -> Route {
    match dispatch(workspace, tool, args) {
        Ok(body) => Route::Json { status: 200, body },
        // The route table only names real tools.
        Err(ToolFailure::UnknownTool) => error_route(500, &format!("no tool {tool}")),
        Err(ToolFailure::BadArguments(message)) => error_route(400, &message),
        // A domain refusal is an outcome, carried with the status that
        // says "understood, refused".
        Err(ToolFailure::Domain(error)) => error_route(422, &error.to_string()),
    }
}

fn parse_args(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn error_route(status: u16, message: &str) -> Route {
    Route::Json {
        status,
        body: json!({"error": message}),
    }
}

fn query_value<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}
