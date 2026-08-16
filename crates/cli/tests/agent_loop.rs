use std::fs;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command as StdCommand, Stdio};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

const TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z";
const SNAPSHOT_ID: &str = "[0-9a-f]{40}";

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

fn assert_line_matches(line: &str, pattern: &str) {
    let matched = predicate::str::is_match(pattern)
        .expect("valid pattern")
        .eval(line);
    assert!(matched, "line {line:?} does not match {pattern:?}");
}

/// A scripted MCP client: the `ws serve --mcp-stdio` child on the other
/// end of a pipe, spoken to one JSON-RPC line at a time.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    fn start(config_home: &Path, workspace: &Path, env: &[(&str, &str)]) -> Self {
        let mut command = StdCommand::new(env!("CARGO_BIN_EXE_ws"));
        command
            .args(["serve", "--mcp-stdio"])
            .current_dir(workspace)
            .env("ATELIER_CONFIG_HOME", config_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn ws serve --mcp-stdio");
        let stdin = child.stdin.take().expect("child stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout is piped"));
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = client.request("initialize", &json!({"protocolVersion": "2025-03-26"}));
        assert_eq!(init["serverInfo"]["name"], "atelier");
        assert_eq!(init["protocolVersion"], "2025-03-26");
        client
            .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .expect("send initialized notification");
        client
    }

    fn send(&mut self, message: &Value) -> std::io::Result<()> {
        writeln!(self.stdin, "{message}")?;
        self.stdin.flush()
    }

    fn send_request(&mut self, method: &str, params: &Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .expect("send request");
        id
    }

    fn recv(&mut self, id: i64) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let message: Value = serde_json::from_str(&line).expect("response is json");
        assert_eq!(message["id"], id, "response answers the request");
        message["result"].clone()
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        let id = self.send_request(method, params);
        self.recv(id)
    }

    /// Call a tool that must succeed; its decoded payload.
    fn call(&mut self, tool: &str, args: &Value) -> Value {
        let (payload, is_error) = self.try_call(tool, args);
        assert!(!is_error, "{tool} failed: {payload}");
        payload
    }

    /// Call a tool, returning its decoded payload (or error text) and
    /// whether the tool reported an error.
    fn try_call(&mut self, tool: &str, args: &Value) -> (Value, bool) {
        let result = self.request("tools/call", &json!({"name": tool, "arguments": args}));
        decode_tool_result(&result)
    }
}

fn decode_tool_result(result: &Value) -> (Value, bool) {
    let is_error = result["isError"].as_bool().expect("isError is present");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result carries text");
    if is_error {
        return (Value::String(text.to_owned()), true);
    }
    (
        serde_json::from_str(text).expect("tool payload is json"),
        false,
    )
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_mcp_client_runs_the_session_loop_end_to_end() {
    let config_home = TempDir::new().expect("create temp config home");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create temp workspace");

    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();
    fs::write(workspace.path().join("notes.txt"), "hello\n").expect("write base file");

    let mut client = McpClient::start(config_home.path(), workspace.path(), &[]);

    let tools = client.request("tools/list", &json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool has a name"))
        .collect();
    assert_eq!(
        names,
        [
            "open_session",
            "read",
            "write",
            "diff",
            "request_land",
            "approve",
            "reject",
            "land",
            "landing_requests",
            "journal",
            "abandon"
        ]
    );

    let session = client.call(
        "open_session",
        &json!({
            "actor_name": "scribe",
            "actor_kind": "agent",
            "instruction_summary": "redline the notes",
            "instruction_run_ref": "bip:run/7",
        }),
    );
    assert_eq!(session["session_id"], "s1");
    let working_copy = session["working_copy"].as_str().expect("working_copy");
    assert!(working_copy.ends_with(".atelier/sessions/s1"));
    assert!(!session["change_id"].as_str().expect("change_id").is_empty());

    let written = client.call(
        "write",
        &json!({"session_id": "s1", "path": "notes.txt", "content": "hello agent\n"}),
    );
    assert_line_matches(
        written["snapshot_id"].as_str().expect("snapshot_id"),
        &format!("^{SNAPSHOT_ID}$"),
    );

    // Write and read round-trip through the same surface.
    let read = client.call("read", &json!({"session_id": "s1", "path": "notes.txt"}));
    assert_eq!(read["content"], "hello agent\n");
    assert_eq!(read["window"], json!({"start": 0, "end": 12, "total": 12}));
    assert_eq!(read["next"], Value::Null);
    assert_eq!(read["projected_by"], Value::Null);

    let diff = client.call("diff", &json!({"session_id": "s1"}));
    assert_eq!(diff["diff"], "M notes.txt\n-hello\n+hello agent");

    let request = client.call("request_land", &json!({"session_id": "s1"}));
    assert_eq!(request["request_id"], "r1");
    assert_eq!(request["state"], "open");

    let outcome = client.call(
        "approve",
        &json!({"request_id": "r1", "actor_name": "scribe", "actor_kind": "agent"}),
    );
    assert_eq!(outcome["state"], "landed");
    let landed = outcome["snapshot_id"].as_str().expect("landed snapshot");

    // The change is on the shared line, attributed to the agent.
    let log = ws(config_home.path(), workspace.path())
        .arg("log")
        .assert()
        .success();
    assert_line_matches(
        &stdout_lines(&log)[0],
        &format!("^{landed}  scribe  {TIMESTAMP}$"),
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes.txt")).expect("read shared file"),
        "hello agent\n"
    );

    ws(config_home.path(), workspace.path())
        .arg("requests")
        .assert()
        .success()
        .stdout("r1  landed  session s1  by scribe (agent)\n");

    ws(config_home.path(), workspace.path())
        .arg("sessions")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match("^s1  landed  scribe \\(agent\\)  [0-9a-z]+\n$")
                .expect("valid pattern"),
        );

    // The journal tells the whole story: the session's acts under its id,
    // the instruction summary and run reference on the opening entry.
    let journal = ws(config_home.path(), workspace.path())
        .arg("journal")
        .assert()
        .success();
    let lines = stdout_lines(&journal);
    assert_eq!(lines.len(), 7, "unexpected journal: {lines:#?}");
    assert_line_matches(
        &lines[0],
        &format!("^{TIMESTAMP}  scribe \\(agent\\)  land  s1  r1 {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        &lines[1],
        &format!("^{TIMESTAMP}  scribe \\(agent\\)  approve  s1  r1 {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        &lines[2],
        &format!("^{TIMESTAMP}  scribe \\(agent\\)  land_request  s1  r1$"),
    );
    assert_line_matches(
        &lines[3],
        &format!("^{TIMESTAMP}  scribe \\(agent\\)  snapshot  s1  {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        &lines[4],
        &format!(
            "^{TIMESTAMP}  scribe \\(agent\\)  session_open  s1  \"redline the notes\"  bip:run/7$"
        ),
    );
    assert_line_matches(
        &lines[5],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  snapshot  {SNAPSHOT_ID}$"),
    );
    assert_line_matches(
        &lines[6],
        &format!("^{TIMESTAMP}  test-actor \\(human\\)  workspace_init$"),
    );

    // A landed session is closed for work.
    let (message, is_error) = client.try_call(
        "write",
        &json!({"session_id": "s1", "path": "notes.txt", "content": "more\n"}),
    );
    assert!(is_error);
    assert_eq!(message, "error: session s1 is landed");
}

const DOCX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const DOCX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

/// A one-paragraph Word document holding `sentence`, zipped as a .docx.
fn docx(sentence: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{sentence}</w:t></w:r></w:p></w:body></w:document>"#
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer
        .start_file("[Content_Types].xml", options)
        .expect("start fixture part");
    writer
        .write_all(DOCX_CONTENT_TYPES.as_bytes())
        .expect("write fixture part");
    writer
        .start_file("_rels/.rels", options)
        .expect("start fixture part");
    writer
        .write_all(DOCX_RELS.as_bytes())
        .expect("write fixture part");
    writer
        .start_file("word/document.xml", options)
        .expect("start fixture part");
    writer
        .write_all(document.as_bytes())
        .expect("write fixture part");
    writer
        .finish()
        .expect("finish fixture archive")
        .into_inner()
}

#[test]
fn mcp_reads_are_windowed_and_documents_read_as_projections() {
    let config_home = TempDir::new().expect("create temp config home");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create temp workspace");

    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    let mut client = McpClient::start(config_home.path(), workspace.path(), &[]);
    let session = client.call(
        "open_session",
        &json!({
            "actor_name": "scribe",
            "actor_kind": "agent",
            "instruction_summary": "read the big report",
        }),
    );
    let working_copy = fs::canonicalize(workspace.path())
        .expect("canonicalize workspace root")
        .join(".atelier/sessions/s1");
    assert_eq!(
        session["working_copy"].as_str().expect("working_copy"),
        working_copy.to_str().expect("utf-8 path")
    );

    // A file larger than one window reads bounded, with a continuation
    // cursor, and the windows reassemble the file exactly.
    let big = "0123456789".repeat(12_000);
    fs::write(working_copy.join("big.txt"), &big).expect("write big file");
    let mut reassembled = String::new();
    let mut cursor = json!(0);
    let mut windows = 0;
    while !cursor.is_null() {
        let read = client.call(
            "read",
            &json!({"session_id": "s1", "path": "big.txt", "start": cursor}),
        );
        assert!(read["content"].as_str().expect("content").len() <= 50_000);
        reassembled.push_str(read["content"].as_str().expect("content"));
        cursor = read["next"].clone();
        windows += 1;
    }
    assert_eq!(windows, 3);
    assert_eq!(reassembled, big);

    // A document with a format package reads as its projection by default.
    fs::write(
        working_copy.join("report.docx"),
        docx("The quick brown fox jumps over the lazy dog."),
    )
    .expect("write docx fixture");
    let read = client.call("read", &json!({"session_id": "s1", "path": "report.docx"}));
    assert_eq!(
        read["content"],
        "The quick brown fox jumps over the lazy dog.\n"
    );
    assert_eq!(read["projected_by"], "format-docx@0.3.0");

    // Opaque bytes without a package refuse instead of degrading.
    fs::write(working_copy.join("blob.bin"), [0xff_u8, 0xfe, 0x00, 0x01])
        .expect("write opaque file");
    let (message, is_error) =
        client.try_call("read", &json!({"session_id": "s1", "path": "blob.bin"}));
    assert!(is_error);
    assert_eq!(
        message,
        "error: no format package projects blob.bin and it is not utf-8 text"
    );

    // Windows past the cap refuse by naming it.
    let (message, is_error) = client.try_call(
        "read",
        &json!({"session_id": "s1", "path": "big.txt", "max_bytes": 50_001}),
    );
    assert!(is_error);
    assert_eq!(message, "error: read windows span 1 to 50000 bytes");

    // Abandon closes the session; the surface says so on the next write.
    let abandoned = client.call("abandon", &json!({"session_id": "s1"}));
    assert_eq!(abandoned["state"], "abandoned");
    let (message, is_error) = client.try_call(
        "write",
        &json!({"session_id": "s1", "path": "big.txt", "content": "x"}),
    );
    assert!(is_error);
    assert_eq!(message, "error: session s1 is abandoned");
}

#[test]
fn concurrent_applies_share_one_lease_across_processes() {
    let config_home = TempDir::new().expect("create temp config home");
    write_actor_config(config_home.path());
    let workspace = TempDir::new().expect("create temp workspace");

    ws(config_home.path(), workspace.path())
        .arg("init")
        .assert()
        .success();

    // The server process holds the landing lease through its apply; the
    // seam keeps it held long enough for the CLI process to collide.
    let mut client = McpClient::start(
        config_home.path(),
        workspace.path(),
        &[("ATELIER_LAND_HOLD_MS", "4000")],
    );
    for (session, file, content) in [("s1", "a.txt", "from s1\n"), ("s2", "b.txt", "from s2\n")] {
        let opened = client.call(
            "open_session",
            &json!({
                "actor_name": "scribe",
                "actor_kind": "agent",
                "instruction_summary": "race the gate",
            }),
        );
        assert_eq!(opened["session_id"], session);
        client.call(
            "write",
            &json!({"session_id": session, "path": file, "content": content}),
        );
        client.call("request_land", &json!({"session_id": session}));
    }

    // The server begins the r1 apply and sits on the lease.
    let approving = client.send_request(
        "tools/call",
        &json!({"name": "approve", "arguments": {
            "request_id": "r1", "actor_name": "scribe", "actor_kind": "agent",
        }}),
    );
    std::thread::sleep(std::time::Duration::from_millis(400));

    // The CLI process loses the claim and learns who holds the point.
    ws(config_home.path(), workspace.path())
        .args(["approve", "r2"])
        .assert()
        .failure()
        .stderr(
            predicate::str::is_match(
                r"^error: the landing lease is held by test-actor:\d+ until \d+\n$",
            )
            .expect("valid pattern"),
        );

    // Exactly one apply ran: the winner lands and releases the point.
    let response = client.recv(approving);
    let (outcome, is_error) = decode_tool_result(&response);
    assert!(!is_error, "the winner's approve failed: {outcome}");
    assert_eq!(outcome["state"], "landed");
    let first = outcome["snapshot_id"]
        .as_str()
        .expect("snapshot")
        .to_owned();

    // The loser succeeds on retry.
    let retry = ws(config_home.path(), workspace.path())
        .args(["approve", "r2"])
        .assert()
        .success();
    let retry_line = stdout_lines(&retry)[0].clone();
    assert_line_matches(&retry_line, &format!("^landed {SNAPSHOT_ID}$"));
    let second = retry_line
        .strip_prefix("landed ")
        .expect("landed line")
        .to_owned();

    // No corruption, no lost snapshot: both changes sit on the shared
    // line in landing order, and both files carry their content.
    let log = ws(config_home.path(), workspace.path())
        .arg("log")
        .assert()
        .success();
    let lines = stdout_lines(&log);
    assert_line_matches(&lines[0], &format!("^{second}  scribe  {TIMESTAMP}$"));
    assert_line_matches(&lines[1], &format!("^{first}  scribe  {TIMESTAMP}$"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("a.txt")).expect("read a.txt"),
        "from s1\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("b.txt")).expect("read b.txt"),
        "from s2\n"
    );
}
