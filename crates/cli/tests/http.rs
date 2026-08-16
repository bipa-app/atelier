//! The HTTP surface end to end: MCP streamable HTTP and REST are reach,
//! not capability — the same verbs, the same core path, the same journal
//! acts as MCP over stdio (ADR-0006).
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command as StdCommand, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

const BOUND: Duration = Duration::from_secs(10);

fn write_actor_config(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
}

fn ws(config_home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("atelier").expect("atelier binary builds");
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(current_dir);
    command
}

fn stdout_lines(assert: &assert_cmd::assert::Assert) -> Vec<String> {
    String::from_utf8(assert.get_output().stdout.clone())
        .expect("stdout is utf-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn init_workspace(config_home: &Path) -> TempDir {
    let workspace = TempDir::new().expect("create workspace tempdir");
    ws(config_home, workspace.path())
        .arg("init")
        .assert()
        .success();
    workspace
}

/// The child's stdout, line by line, off a reader thread so waits can
/// carry a deadline.
fn read_lines(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// A running `atelier serve --http` child and the address it landed on.
struct HttpServer {
    child: Child,
    address: String,
}

impl HttpServer {
    fn spawn(config_home: &Path, workspace: &Path) -> Self {
        Self::spawn_with(config_home, workspace, &[])
    }

    fn spawn_with(config_home: &Path, workspace: &Path, extra_args: &[&str]) -> Self {
        let mut child = StdCommand::new(env!("CARGO_BIN_EXE_atelier"))
            .args(["serve", "--http", "--bind", "127.0.0.1:0"])
            .args(extra_args)
            .env("ATELIER_CONFIG_HOME", config_home)
            .current_dir(workspace)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn atelier serve --http");
        let lines = read_lines(child.stdout.take().expect("server stdout is piped"));
        let banner = lines
            .recv_timeout(BOUND)
            .expect("the server announces its address");
        assert!(
            banner.starts_with("listening on http://"),
            "unexpected banner: {banner:?}"
        );
        let address = banner["listening on http://".len()..].to_owned();
        Self { child, address }
    }

    /// One HTTP round trip, hand-rolled over a socket: one request per
    /// connection needs no client stack.
    fn request(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        self.request_as(method, path, body, None)
    }

    /// The same round trip, optionally carrying an Authorization header.
    fn request_as(
        &self,
        method: &str,
        path: &str,
        body: &str,
        bearer: Option<&str>,
    ) -> (u16, String) {
        let authorization = bearer.map_or(String::new(), |token| {
            format!("Authorization: Bearer {token}\r\n")
        });
        let mut stream = TcpStream::connect(&self.address).expect("connect to the server");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.address,
            body.len(),
        )
        .expect("write the request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read the response");
        let status: u16 = response
            .split_whitespace()
            .nth(1)
            .expect("a status code")
            .parse()
            .expect("status code is numeric");
        let body = response
            .split_once("\r\n\r\n")
            .expect("headers end")
            .1
            .to_owned();
        (status, body)
    }

    fn json(&self, method: &str, path: &str, body: &Value) -> Value {
        let (status, body) = self.request(method, path, &body.to_string());
        assert_eq!(status, 200, "unexpected status; body: {body}");
        serde_json::from_str(&body).expect("response is json")
    }

    /// One MCP tool call over streamable HTTP; the decoded payload.
    fn call(&self, id: i64, tool: &str, args: &Value) -> Value {
        let message = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        let response = self.json("POST", "/mcp", &message);
        assert_eq!(response["id"], id);
        decode_tool_result(&response["result"])
    }

    fn kill(mut self) {
        self.child.kill().expect("kill atelier serve --http");
        self.child.wait().expect("reap atelier serve --http");
    }
}

fn decode_tool_result(result: &Value) -> Value {
    assert_eq!(result["isError"], false, "tool failed: {result}");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result is text");
    serde_json::from_str(text).expect("tool payload is json")
}

/// A `atelier serve --mcp-stdio` child spoken to one JSON-RPC line at a time.
struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioClient {
    fn start(config_home: &Path, workspace: &Path) -> Self {
        let mut child = StdCommand::new(env!("CARGO_BIN_EXE_atelier"))
            .args(["serve", "--mcp-stdio"])
            .env("ATELIER_CONFIG_HOME", config_home)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn atelier serve --mcp-stdio");
        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout is piped"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn call(&mut self, id: i64, tool: &str, args: &Value) -> Value {
        let message = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        });
        writeln!(self.stdin, "{message}").expect("write the request");
        self.stdin.flush().expect("flush the request");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read the response");
        let response: Value = serde_json::from_str(&line).expect("response is json");
        assert_eq!(response["id"], id);
        decode_tool_result(&response["result"])
    }

    fn stop(mut self) {
        drop(self.stdin);
        self.child.wait().expect("reap atelier serve --mcp-stdio");
    }
}

#[test]
fn curl_returns_the_same_diff_ws_diff_prints() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());
    fs::write(workspace.path().join("notes.txt"), "hello\n").expect("write notes");
    // Snapshot the first version, then change it: a changed file raises
    // to a line diff, where an added one keeps its listing line.
    ws(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    fs::write(workspace.path().join("notes.txt"), "hello\nworld\n").expect("append notes");

    let server = HttpServer::spawn(config_home.path(), workspace.path());
    let curled = StdCommand::new("curl")
        .args(["-s", &format!("http://{}/v1/diff", server.address)])
        .output()
        .expect("curl the diff");
    server.kill();
    assert!(curled.status.success());

    let cli = ws(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success();
    let cli_diff = String::from_utf8(cli.get_output().stdout.clone()).expect("diff is utf-8");
    let curled_diff = String::from_utf8(curled.stdout).expect("curled diff is utf-8");
    assert_eq!(curled_diff, "M notes.txt\n+world\n");
    assert_eq!(curled_diff, cli_diff);
}

#[test]
fn an_mcp_http_client_runs_the_full_session_loop() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());

    let server = HttpServer::spawn(config_home.path(), workspace.path());

    let init = server.json(
        "POST",
        "/mcp",
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "atelier");

    let session = server.call(
        2,
        "open_session",
        &json!({
            "actor_name": "loop-agent", "actor_kind": "agent",
            "instruction_summary": "draft the plan over http",
        }),
    );
    assert_eq!(session["session_id"], "s1");

    let written = server.call(
        3,
        "write",
        &json!({"session_id": "s1", "path": "plan.md", "content": "# Plan\n\ndraft one\n"}),
    );
    assert!(
        written["snapshot_id"]
            .as_str()
            .is_some_and(|id| id.len() == 40)
    );

    let diff = server.call(4, "diff", &json!({"session_id": "s1"}));
    assert_eq!(diff["diff"], "A plan.md");

    let landed = server.call(5, "land", &json!({"session_id": "s1"}));
    assert_eq!(landed["state"], "landed");
    let snapshot = landed["landings"][0]["snapshot_id"]
        .as_str()
        .expect("landed snapshot id")
        .to_owned();

    server.kill();

    // The change is on the shared line, visible to the human face, and
    // the landed content is in the working copy.
    let log = ws(config_home.path(), workspace.path())
        .arg("history")
        .assert()
        .success();
    let head = stdout_lines(&log).remove(0);
    assert!(
        head.starts_with(&snapshot) && head.contains("loop-agent"),
        "unexpected head: {head:?}"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("plan.md")).expect("landed file"),
        "# Plan\n\ndraft one\n"
    );
}

#[test]
fn every_transport_records_identical_journal_acts() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let actor = json!({
        "actor_name": "parity-agent", "actor_kind": "agent",
        "instruction_summary": "the parity check",
    });

    // MCP over stdio.
    let stdio_workspace = init_workspace(config_home.path());
    let mut stdio = StdioClient::start(config_home.path(), stdio_workspace.path());
    let session = stdio.call(1, "open_session", &actor);
    assert_eq!(session["session_id"], "s1");
    stdio.call(
        2,
        "write",
        &json!({"session_id": "s1", "path": "plan.md", "content": "draft\n"}),
    );
    let landed = stdio.call(3, "land", &json!({"session_id": "s1"}));
    assert_eq!(landed["state"], "landed");
    stdio.stop();

    // MCP over streamable HTTP.
    let http_workspace = init_workspace(config_home.path());
    let server = HttpServer::spawn(config_home.path(), http_workspace.path());
    let session = server.call(1, "open_session", &actor);
    assert_eq!(session["session_id"], "s1");
    server.call(
        2,
        "write",
        &json!({"session_id": "s1", "path": "plan.md", "content": "draft\n"}),
    );
    let landed = server.call(3, "land", &json!({"session_id": "s1"}));
    assert_eq!(landed["state"], "landed");
    server.kill();

    // Plain REST.
    let rest_workspace = init_workspace(config_home.path());
    let server = HttpServer::spawn(config_home.path(), rest_workspace.path());
    let session = server.json("POST", "/v1/sessions", &actor);
    assert_eq!(session["session_id"], "s1");
    let (status, _) = server.request("PUT", "/v1/sessions/s1/files/plan.md", "draft\n");
    assert_eq!(status, 200);
    let (status, landed) = server.request("POST", "/v1/sessions/s1/land", "");
    assert_eq!(status, 200);
    let landed: Value = serde_json::from_str(&landed).expect("land response is json");
    assert_eq!(landed["state"], "landed");
    server.kill();

    // One capability, three transports, act-for-act identical journals —
    // read through the fourth face, the CLI.
    let stdio_acts = normalized_journal(config_home.path(), stdio_workspace.path());
    let http_acts = normalized_journal(config_home.path(), http_workspace.path());
    let rest_acts = normalized_journal(config_home.path(), rest_workspace.path());
    assert_eq!(
        stdio_acts,
        vec![
            "T  parity-agent (agent)  land  s1  r1 ID",
            "T  parity-agent (agent)  approve  s1  r1 ID",
            "T  parity-agent (agent)  land_request  s1  r1",
            "T  parity-agent (agent)  snapshot  s1  ID",
            "T  parity-agent (agent)  session_open  s1  \"the parity check\"",
            "T  test-actor (human)  workspace_init",
        ],
    );
    assert_eq!(stdio_acts, http_acts);
    assert_eq!(stdio_acts, rest_acts);
}

/// The workspace's journal as `atelier journal` renders it, timestamps and
/// snapshot ids normalized so runs compare across workspaces.
fn normalized_journal(config_home: &Path, workspace: &Path) -> Vec<String> {
    let journal = ws(config_home, workspace).arg("journal").assert().success();
    stdout_lines(&journal)
        .iter()
        .map(|line| replace_hex_runs(&replace_timestamp(line)))
        .collect()
}

/// The leading rfc3339 timestamp as `T`.
fn replace_timestamp(line: &str) -> String {
    match line.split_once("Z  ") {
        Some((_, rest)) => format!("T  {rest}"),
        None => line.to_owned(),
    }
}

/// Every 40-hex run (a snapshot id) as `ID`.
fn replace_hex_runs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    for c in line.chars() {
        if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run.push(c);
            continue;
        }
        flush_hex_run(&mut out, &mut run);
        out.push(c);
    }
    flush_hex_run(&mut out, &mut run);
    out
}

fn flush_hex_run(out: &mut String, run: &mut String) {
    if run.len() == 40 {
        out.push_str("ID");
    } else {
        out.push_str(run);
    }
    run.clear();
}

#[test]
fn a_bind_beyond_loopback_needs_the_explicit_flag() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());

    ws(config_home.path(), workspace.path())
        .args(["serve", "--http", "--bind", "0.0.0.0:0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --allow-remote to mean it"));

    // The flag alone is not enough: beyond loopback a token is mandatory.
    ws(config_home.path(), workspace.path())
        .args(["serve", "--http", "--bind", "0.0.0.0:0", "--allow-remote"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires --token"));

    // With the flag and a token the same bind starts and announces itself.
    let mut child = StdCommand::new(env!("CARGO_BIN_EXE_atelier"))
        .args([
            "serve",
            "--http",
            "--bind",
            "0.0.0.0:0",
            "--allow-remote",
            "--token",
            "hush",
        ])
        .env("ATELIER_CONFIG_HOME", config_home.path())
        .current_dir(workspace.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn atelier serve --http --allow-remote");
    let lines = read_lines(child.stdout.take().expect("server stdout is piped"));
    let banner = lines.recv_timeout(BOUND).expect("the server announces");
    assert!(
        banner.starts_with("listening on http://0.0.0.0:"),
        "unexpected banner: {banner:?}"
    );
    child.kill().expect("kill the server");
    child.wait().expect("reap the server");
}

/// A one-paragraph Word document whose run carries `rpr`, zipped as .docx.
fn formatted_docx(rpr: &str, sentence: &str) -> Vec<u8> {
    use std::io::Cursor;
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr>{rpr}</w:rPr><w:t>{sentence}</w:t></w:r></w:p></w:body></w:document>"#
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer
        .start_file("word/document.xml", options)
        .expect("start fixture part");
    std::io::Write::write_all(&mut writer, document.as_bytes()).expect("write fixture part");
    writer
        .finish()
        .expect("finish fixture archive")
        .into_inner()
}

#[test]
fn a_rich_docx_delta_reads_the_same_over_http_and_the_cli() {
    // The rich rung crosses the wire: a formatting-only edit — invisible
    // to any line diff — reaches a curl script byte-for-byte as the CLI
    // prints it, summary line included.
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());
    fs::write(
        workspace.path().join("report.docx"),
        formatted_docx(r#"<w:sz w:val="22"/>"#, "resize this clause"),
    )
    .expect("write docx");
    ws(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    fs::write(
        workspace.path().join("report.docx"),
        formatted_docx(r#"<w:sz w:val="28"/>"#, "resize this clause"),
    )
    .expect("resize docx");

    let server = HttpServer::spawn(config_home.path(), workspace.path());
    let (status, body) = server.request("GET", "/v1/diff", "");
    server.kill();

    assert_eq!(status, 200);
    assert_eq!(
        body,
        "M report.docx\nM report.docx > paragraph 1\n  \"resize this clause\" font size 11 → 14\n"
    );
    let cli = ws(config_home.path(), workspace.path())
        .arg("diff")
        .assert()
        .success();
    let cli_diff = String::from_utf8(cli.get_output().stdout.clone()).expect("diff is utf-8");
    assert_eq!(body, cli_diff);
}

#[test]
fn the_error_surface_answers_by_the_book() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());
    let server = HttpServer::spawn(config_home.path(), workspace.path());

    // Routing: unknown resources 404, a stream request 405.
    let (status, body) = server.request("GET", "/nope", "");
    assert_eq!(
        (status, body.as_str()),
        (404, "{\"error\":\"no such resource\"}")
    );
    let (status, _) = server.request("GET", "/mcp", "");
    assert_eq!(status, 405);

    // Broken protocol answers 400: a session open without its actor, a
    // journal limit that is not a number.
    let (status, _) = server.request("POST", "/v1/sessions", "{}");
    assert_eq!(status, 400);
    let (status, body) = server.request("GET", "/v1/journal?limit=abc", "");
    assert_eq!(
        (status, body.as_str()),
        (400, "{\"error\":\"limit must be a number\"}")
    );

    // Domain refusals answer 422 and name the refusal.
    let (status, body) = server.request("GET", "/v1/sessions/s99/diff", "");
    assert_eq!(status, 422);
    assert_eq!(body, "{\"error\":\"no session s99\"}");

    // A working-copy escape refuses like any domain rule.
    let session = server.json(
        "POST",
        "/v1/sessions",
        &serde_json::json!({
            "actor_name": "edge-agent", "actor_kind": "agent",
            "instruction_summary": "probe the error surface",
        }),
    );
    assert_eq!(session["session_id"], "s1");
    let (status, body) = server.request("PUT", "/v1/sessions/s1/files/../escape.txt", "gotcha");
    assert_eq!(status, 422);
    assert_eq!(
        body,
        "{\"error\":\"path ../escape.txt leaves the session working copy\"}"
    );

    // MCP over HTTP: a notification is consumed (202, empty), a parse
    // error answers as JSON-RPC, not as transport failure.
    let (status, body) = server.request(
        "POST",
        "/mcp",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}",
    );
    assert_eq!((status, body.as_str()), (202, ""));
    let (status, body) = server.request("POST", "/mcp", "this is not json");
    assert_eq!(status, 200);
    let message: Value = serde_json::from_str(&body).expect("json-rpc error is json");
    assert_eq!(message["error"]["code"], -32700);

    // A body past the cap answers 413 before any dispatch.
    let oversized = "x".repeat(8 * 1024 * 1024 + 1);
    let (status, _) = server.request("PUT", "/v1/sessions/s1/files/big.txt", &oversized);
    assert_eq!(status, 413);

    server.kill();
}

#[test]
fn rest_sessions_take_mount_scoped_paths_unchanged_in_shape() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());
    let app = TempDir::new().expect("create app source");
    fs::write(app.path().join("main.rs"), "fn main() {}\n").expect("write app file");
    ws(config_home.path(), workspace.path())
        .args(["attach", app.path().to_str().expect("utf-8 path")])
        .args(["--mount", "app"])
        .assert()
        .success();

    let server = HttpServer::spawn(config_home.path(), workspace.path());
    let session = server.json(
        "POST",
        "/v1/sessions",
        &serde_json::json!({
            "actor_name": "span-agent", "actor_kind": "agent",
            "instruction_summary": "edit a mounted project over rest",
        }),
    );
    assert_eq!(session["session_id"], "s1");

    let (status, written) = server.request(
        "PUT",
        "/v1/sessions/s1/files/app/main.rs",
        "fn main() { run() }\n",
    );
    assert_eq!(status, 200);
    let written: Value = serde_json::from_str(&written).expect("write response is json");
    assert!(
        written["snapshot_id"]
            .as_str()
            .is_some_and(|id| id.len() == 40)
    );

    let diff = server.json("GET", "/v1/sessions/s1/diff", &Value::Null);
    assert_eq!(
        diff["diff"],
        "M app/main.rs\n-fn main() {}\n+fn main() { run() }"
    );
    server.kill();
}

#[test]
fn rest_speaks_every_verb_the_tools_speak() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());
    fs::write(workspace.path().join("notes.txt"), "hello\n").expect("write base file");
    let server = HttpServer::spawn(config_home.path(), workspace.path());

    // The read models arrive as the exact text the CLI prints.
    let (status, manifest) = server.request("GET", "/v1/manifest", "");
    assert_eq!(status, 200);
    assert!(manifest.starts_with("workspace: "), "got: {manifest}");
    let (status, state) = server.request("GET", "/v1/status", "");
    assert_eq!(status, 200);
    assert!(state.starts_with("head: "), "got: {state}");

    // One session through the whole gate, REST alone.
    let session = server.json(
        "POST",
        "/v1/sessions",
        &json!({
            "actor_name": "rest-agent", "actor_kind": "agent",
            "instruction_summary": "revise the notes over rest",
        }),
    );
    assert_eq!(session["session_id"], "s1");
    let (status, _) = server.request("PUT", "/v1/sessions/s1/files/notes.txt", "hello\nworld\n");
    assert_eq!(status, 200);
    let read = server.json(
        "GET",
        "/v1/sessions/s1/files/notes.txt?max_bytes=6",
        &json!({}),
    );
    assert_eq!(read["content"], "hello\n");
    assert_eq!(read["next"], 6);
    let request = server.json("POST", "/v1/sessions/s1/request-land", &json!({}));
    assert_eq!(request["request_id"], "r1");
    assert_eq!(request["state"], "open");
    let requests = server.json("GET", "/v1/requests", &json!({}));
    assert_eq!(requests["requests"][0]["request_id"], "r1");
    let outcome = server.json(
        "POST",
        "/v1/requests/r1/approve",
        &json!({"actor_name": "approver", "actor_kind": "human"}),
    );
    assert_eq!(outcome["state"], "landed");

    // Undo re-opens the gate over REST; approving again lands again.
    let undone = server.json("POST", "/v1/requests/r1/undo", &json!({}));
    assert_eq!(undone["state"], "open");
    assert_eq!(undone["restored"][0]["source"], Value::Null);
    let relanded = server.json(
        "POST",
        "/v1/requests/r1/approve",
        &json!({"actor_name": "approver", "actor_kind": "human"}),
    );
    assert_eq!(relanded["state"], "landed");

    // A second session rejects; a third abandons.
    server.json(
        "POST",
        "/v1/sessions",
        &json!({
            "actor_name": "rest-agent", "actor_kind": "agent",
            "instruction_summary": "a change to refuse",
        }),
    );
    let (status, _) = server.request("PUT", "/v1/sessions/s2/files/notes.txt", "noise\n");
    assert_eq!(status, 200);
    server.json("POST", "/v1/sessions/s2/request-land", &json!({}));
    let rejected = server.json(
        "POST",
        "/v1/requests/r2/reject",
        &json!({"reason": "not like this"}),
    );
    assert_eq!(rejected["state"], "rejected");

    // A rejected request refuses to undo, 422 with the reason by name.
    let (status, body) = server.request("POST", "/v1/requests/r2/undo", "{}");
    assert_eq!(status, 422, "got: {body}");
    assert!(body.contains("only a landed request undoes"), "got: {body}");
    server.json(
        "POST",
        "/v1/sessions",
        &json!({
            "actor_name": "rest-agent", "actor_kind": "agent",
            "instruction_summary": "a change to walk away from",
        }),
    );
    let abandoned = server.json("POST", "/v1/sessions/s3/abandon", &json!({}));
    assert_eq!(abandoned["state"], "abandoned");

    server.kill();
}

#[test]
fn http_auth_refuses_without_the_token() {
    let config_home = TempDir::new().expect("create config tempdir");
    write_actor_config(config_home.path());
    let workspace = init_workspace(config_home.path());
    let server = HttpServer::spawn_with(config_home.path(), workspace.path(), &["--token", "hush"]);

    // No token, wrong token: 401 on both faces. The right one: 200.
    let (status, body) = server.request("GET", "/v1/status", "");
    assert_eq!(status, 401, "got: {body}");
    let (status, _) = server.request_as("GET", "/v1/status", "", Some("wrong"));
    assert_eq!(status, 401);
    let (status, _) = server.request_as("POST", "/mcp", "{}", None);
    assert_eq!(status, 401);
    let (status, body) = server.request_as("GET", "/v1/status", "", Some("hush"));
    assert_eq!(status, 200);
    assert!(body.starts_with("head: "), "got: {body}");
    server.kill();

    // Beyond loopback, a token is not optional: the server refuses to start.
    let refused = StdCommand::new(env!("CARGO_BIN_EXE_atelier"))
        .args(["serve", "--http", "--bind", "0.0.0.0:0", "--allow-remote"])
        .env("ATELIER_CONFIG_HOME", config_home.path())
        .current_dir(workspace.path())
        .output()
        .expect("run atelier serve");
    assert!(!refused.status.success());
    let stderr = String::from_utf8(refused.stderr).expect("stderr is utf-8");
    assert!(stderr.contains("requires --token"), "got: {stderr}");
}
