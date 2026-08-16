use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;

use atelier_core::{Error, Workspace, render_diff};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::mcp::{ToolFailure, dispatch, handle_message};

/// The most an HTTP request body may carry. Requests hold file content and
/// JSON-RPC messages; anything past this is not a workspace operation.
const MAX_BODY_SIZE: usize = 8 * 1024 * 1024;

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
pub fn serve_http(root: &Path, bind: &str, allow_remote: bool) -> Result<(), Error> {
    let address: SocketAddr = bind
        .parse()
        .map_err(|_| Error::Config(format!("bind address {bind:?} is not ip:port")))?;
    if !address.ip().is_loopback() && !allow_remote {
        return Err(Error::Config(format!(
            "binding {address} exposes the workspace beyond this machine; pass --allow-remote to mean it"
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

    for mut request in server.incoming_requests() {
        let reply = reply(&mut workspace, &mut request, &content_types);
        // A client gone before its response is its loss, never the
        // server's: the loop serves the next request.
        let _ = request.respond(reply);
    }
    Ok(())
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

fn body(request: &mut Request) -> Result<String, Response<std::io::Cursor<Vec<u8>>>> {
    let mut body = String::new();
    let mut reader = request.as_reader().take(MAX_BODY_SIZE as u64 + 1);
    if reader.read_to_string(&mut body).is_err() {
        return Err(Response::from_string(
            json!({"error": "the request body is not utf-8"}).to_string(),
        )
        .with_status_code(400));
    }
    if body.len() > MAX_BODY_SIZE {
        return Err(Response::from_string(
            json!({"error": format!("the request body exceeds {MAX_BODY_SIZE} bytes")}).to_string(),
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
            // The exact lines `ws diff` prints for the same snapshots:
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
        _ => session_route(workspace, method, path, body),
    }
}

/// The `/v1/sessions/{id}/…` routes: the same verbs the MCP tools speak.
fn session_route(workspace: &mut Workspace, method: &Method, path: &str, body: &str) -> Route {
    let Some(rest) = path.strip_prefix("/v1/sessions/") else {
        return error_route(404, "no such resource");
    };
    let Some((id, action)) = rest.split_once('/') else {
        return error_route(404, "no such resource");
    };
    match (method, action) {
        (Method::Get, "diff") => json_call(workspace, "diff", &json!({"session_id": id})),
        (Method::Post, "land") => json_call(workspace, "land", &json!({"session_id": id})),
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
