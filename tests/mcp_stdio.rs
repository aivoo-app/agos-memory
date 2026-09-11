//! Integration: `serve --stdio` speaks JSON-RPC over stdin/stdout.
//!
//! Spawns the real binary (`CARGO_BIN_EXE_agos-memory serve --stdio`) with a
//! hermetic config (`provider = "none"`, temp DB) and drives the MCP
//! protocol over the child's stdin/stdout. Tracing logs go to stderr (see
//! `observe::init_tracing`), so stdout carries JSON-RPC only.
//!
//! rmcp answers requests *concurrently*, so tests must run a real sequential
//! session: send one request, wait for its response **by id**, then send the
//! next (a write-then-read race would be a test bug, not a product bug).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

/// How long to wait for any single response before failing.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    responses: Receiver<serde_json::Value>,
    stderr: Receiver<String>,
}

impl Session {
    /// Spawn `serve --stdio` with a hermetic config in `dir` and pump its
    /// stdout into a channel (stderr into one too, for failure diagnostics).
    fn spawn(dir: &std::path::Path) -> Self {
        let cfg = dir.join("mcp_stdio.toml");
        std::fs::write(&cfg, "[embed]\nprovider = \"none\"\n").unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_agos-memory"))
            .args(["serve", "--stdio", "--config"])
            .arg(&cfg)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve --stdio");

        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line.trim())
                    && tx.send(val).is_err()
                {
                    break;
                }
            }
        });

        let stderr = child.stderr.take().unwrap();
        let (etx, erx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut buf);
            let _ = etx.send(buf);
        });

        Self {
            stdin: child.stdin.take().unwrap(),
            child,
            responses: rx,
            stderr: erx,
        }
    }

    /// Send one JSON-RPC request line.
    fn send(&mut self, request: &str) {
        self.stdin
            .write_all(request.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .expect("write request to serve --stdio");
    }

    /// Receive the response with JSON-RPC `id` (skipping any interleaved
    /// server-initiated messages, of which this server sends none).
    fn recv(&self, id: i64) -> serde_json::Value {
        loop {
            match self.responses.recv_timeout(RESPONSE_TIMEOUT) {
                Ok(resp) if resp["id"].as_i64() == Some(id) => return resp,
                Ok(_) => continue, // someone else's response/notification
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for id {id}"),
                Err(RecvTimeoutError::Disconnected) => {
                    let stderr = self.stderr.try_recv().unwrap_or_default();
                    panic!("server closed before answering id {id}\nserver stderr: {stderr}");
                }
            }
        }
    }

    /// Request-response roundtrip: send, then wait for this id's response.
    fn call(&mut self, id: i64, request: &str) -> serde_json::Value {
        self.send(request);
        self.recv(id)
    }

    /// Close stdin (EOF) and wait for the server to exit cleanly.
    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait().expect("wait on serve --stdio") {
                Some(_) => break,
                None if std::time::Instant::now() > deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("serve --stdio did not exit after stdin EOF");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// The initialize handshake request body.
const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

/// The client → server initialized notification (MCP requires it before
/// `tools/call` on protocol versions from 2025-03-26).
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

/// The first text content block of a tool-call response.
fn text_of(resp: &serde_json::Value) -> String {
    resp["result"]["content"]
        .as_array()
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b["type"] == "text")
                .and_then(|b| b["text"].as_str())
        })
        .unwrap_or_default()
        .to_string()
}

#[test]
fn stdio_handshake_init() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::spawn(dir.path());

    let resp = session.call(1, INIT);
    assert_eq!(resp["jsonrpc"], "2.0", "{resp:?}");
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "{resp:?}"
    );
    // 0001 acceptance: the wire identity is this crate, not rmcp's build env.
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "agos-memory",
        "{resp:?}"
    );
    assert_eq!(
        resp["result"]["serverInfo"]["version"],
        agos_memory::VERSION,
        "{resp:?}"
    );

    session.shutdown();
}

#[test]
fn stdio_lists_all_eight_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::spawn(dir.path());
    session.call(1, INIT);
    session.send(INITIALIZED);

    let resp = session.call(
        2,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    );
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "remember",
        "recall",
        "forget",
        "summarize",
        "explain",
        "status",
        "pin",
        "unpin",
    ] {
        assert!(names.contains(&want), "missing {want}: {names:?}");
    }
    assert_eq!(names.len(), 8, "unexpected tools: {names:?}");

    session.shutdown();
}

#[test]
fn stdio_remember_then_recall_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::spawn(dir.path());
    session.call(1, INIT);
    session.send(INITIALIZED);

    let remembered = session.call(
        3,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remember","arguments":{"text":"The test agent prefers dark mode."}}}"#,
    );
    let text = text_of(&remembered);
    assert!(
        text.contains("memory: ") && text.contains("(tier="),
        "remember text: {text}"
    );
    assert!(
        remembered["result"]["structuredContent"]["public_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "remember must return a public_id: {remembered:?}"
    );

    // `remember` defaults to the episodic tier and episodic is opt-in for
    // recall (D26), so the recall must opt in.
    let recalled = session.call(
        4,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall","arguments":{"text":"The test agent prefers dark mode.","include_episodic":true}}}"#,
    );
    let text = text_of(&recalled);
    assert!(
        text.contains("dark mode"),
        "recall must surface the remembered text: {text}"
    );

    session.shutdown();
}

#[test]
fn stdio_recall_finds_remembered_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::spawn(dir.path());
    session.call(1, INIT);
    session.send(INITIALIZED);

    session.call(
        3,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remember","arguments":{"text":"The test agent prefers dark mode."}}}"#,
    );
    let recalled = session.call(
        4,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall","arguments":{"text":"The test agent prefers dark mode.","include_episodic":true}}}"#,
    );
    let report = recalled["result"]["structuredContent"]
        .as_object()
        .expect("structured content");
    assert_eq!(report["no_hit"], false, "{report:?}");
    assert!(
        !report["hits"].as_array().unwrap().is_empty(),
        "degraded BM25 must find the remembered text: {report:?}"
    );

    session.shutdown();
}
