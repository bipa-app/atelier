//! The HTTP surface end to end: MCP streamable HTTP and REST are reach,
//! not capability — the same verbs, the same core path, the same journal
//! acts as MCP over stdio (ADR-0006).

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
    let mut command = Command::cargo_bin("ws").expect("ws binary builds");
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

/// A running `ws serve --http` child and the address it landed on.
struct HttpServer {
    child: Child,
    address: String,
}

impl HttpServer {
    fn spawn(config_home: &Path, workspace: &Path) -> Self {
        let mut child = StdCommand::new(env!("CARGO_BIN_EXE_ws"))
            .args(["serve", "--http", "--bind", "127.0.0.1:0"])
            .env("ATELIER_CONFIG_HOME", config_home)
            .current_dir(workspace)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn ws serve --http");
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
        let mut stream = TcpStream::connect(&self.address).expect("connect to the server");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        self.child.kill().expect("kill ws serve --http");
        self.child.wait().expect("reap ws serve --http");
    }
}

fn decode_tool_result(result: &Value) -> Value {
    assert_eq!(result["isError"], false, "tool failed: {result}");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result is text");
    serde_json::from_str(text).expect("tool payload is json")
}

/// A `ws serve --mcp-stdio` child spoken to one JSON-RPC line at a time.
struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioClient {
    fn start(config_home: &Path, workspace: &Path) -> Self {
        let mut child = StdCommand::new(env!("CARGO_BIN_EXE_ws"))
            .args(["serve", "--mcp-stdio"])
            .env("ATELIER_CONFIG_HOME", config_home)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn ws serve --mcp-stdio");
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
        self.child.wait().expect("reap ws serve --mcp-stdio");
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
    let snapshot = landed["snapshot_id"]
        .as_str()
        .expect("landed snapshot id")
        .to_owned();

    server.kill();

    // The change is on the shared line, visible to the human face, and
    // the landed content is in the working copy.
    let log = ws(config_home.path(), workspace.path())
        .arg("log")
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

/// The workspace's journal as `ws journal` renders it, timestamps and
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

    // With the flag the same bind starts and announces itself.
    let mut child = StdCommand::new(env!("CARGO_BIN_EXE_ws"))
        .args(["serve", "--http", "--bind", "0.0.0.0:0", "--allow-remote"])
        .env("ATELIER_CONFIG_HOME", config_home.path())
        .current_dir(workspace.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ws serve --http --allow-remote");
    let lines = read_lines(child.stdout.take().expect("server stdout is piped"));
    let banner = lines.recv_timeout(BOUND).expect("the server announces");
    assert!(
        banner.starts_with("listening on http://0.0.0.0:"),
        "unexpected banner: {banner:?}"
    );
    child.kill().expect("kill the server");
    child.wait().expect("reap the server");
}
