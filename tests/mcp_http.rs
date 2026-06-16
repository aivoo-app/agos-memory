//! Integration: `serve` (default HTTP) speaks MCP streamable-HTTP over axum.
//!
//! Spawns the real binary with a hermetic config on an ephemeral port and
//! exercises the auth layer (401 without/with a wrong token, 200 with) plus a
//! remember→recall roundtrip over the wire. Hermetic (`provider = "none"`).
//!
//! The wire is the real SEP-2567 flow: `initialize` returns
//! `mcp-session-id` + an SSE stream carrying the JSON-RPC response; every
//! later request carries the session id. Responses on the SSE stream are
//! matched by `id` (rmcp answers concurrently).
//!
//! Note on coverage: a *tokenless* non-loopback bind cannot be exercised over
//! the wire from this sandbox — `Config::validate()` (and the server's own
//! `validate_server_token`) refuse to start it. That rejection path is
//! unit-tested in `src/server/auth.rs`
//! (`authorize_tokenless_only_serves_loopback_binds`); here we cover the
//! token-enforcement paths.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agos-memory"))
}

/// A spawned `serve` child that kills and reaps itself when it leaves scope
/// (only if it is still running), so a panicking test can't orphan a server.
struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

impl std::ops::Deref for Server {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Server {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Spawn `serve` (HTTP) with a hermetic config in `dir`, bound to
/// `127.0.0.1:0` (ephemeral port). Returns the [`Server`] guard and the MCP endpoint URL.
///
/// The child's stderr is drained by a background thread for the child's whole
/// lifetime: dropping the read end would SIGPIPE the server on its next log
/// line (the binary resets SIGPIPE to the default disposition). The channel
/// doubles as the wait mechanism for the `listening on` line and as the
/// failure diagnostics sink.
fn spawn_http(
    dir: &std::path::Path,
    token: Option<&str>,
) -> (Server, String, mpsc::Receiver<String>) {
    let cfg = dir.join("mcp_http.toml");
    let mut cfg_str =
        String::from("[embed]\nprovider = \"none\"\n\n[server]\nbind = \"127.0.0.1:0\"\n");
    if let Some(t) = token {
        cfg_str.push_str(&format!("token = \"{t}\"\n"));
    }
    std::fs::write(&cfg, cfg_str).unwrap();

    let mut child = binary()
        .args(["serve", "--config"])
        .arg(&cfg)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve --http");

    // Forward every stderr line to the test thread and keep draining to EOF.
    // The returned receiver MUST stay alive for the child's whole lifetime:
    // dropping it would make `send` fail, exit the drain thread, and SIGPIPE
    // the server on its next log line (the binary resets SIGPIPE to default).
    // `mpsc::channel` is unbounded so holding the receiver without reading
    // still keeps the pipe drained.
    let stderr = child.stderr.take().unwrap();
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break; // test thread gone; the child is being killed anyway
            }
        }
    });

    // Wait (bounded) for the listen line.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut log = String::new();
    let addr = loop {
        let line = match lines_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    Instant::now() < deadline,
                    "server never printed its listen line; stderr so far:\n{log}"
                );
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("server stderr closed before binding; stderr so far:\n{log}")
            }
        };
        log.push_str(&line);
        log.push('\n');
        if let Some(addr) = line
            .split("listening on ")
            .nth(1)
            // Strip any trailing annotation like " (MCP + JSON API)".
            .and_then(|rest| rest.split('(').next())
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            break addr.to_string();
        }
    };

    (Server(child), format!("http://{addr}/mcp"), lines_rx)
}

/// One client session against the running server.
struct HttpMcp {
    url: String,
    token: Option<String>,
    session: Option<String>,
}

impl HttpMcp {
    fn new(url: String, token: Option<&str>) -> Self {
        Self {
            url,
            token: token.map(String::from),
            session: None,
        }
    }

    fn request(&self, body: &str) -> reqwest::blocking::RequestBuilder {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build client");
        let mut req = client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-03-26")
            .body(body.to_string());
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        req
    }

    /// POST a request and return `(status, json)` — the JSON-RPC payload comes
    /// either as the body or as a `data:` event on the SSE stream.
    fn call(&mut self, body: &str) -> (u16, serde_json::Value) {
        let resp = self.request(body).send().expect("http post");
        let code = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session = Some(s.to_string());
        }

        if content_type.contains("text/event-stream") {
            // The response arrives as SSE events; read until our id shows up.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut reader = BufReader::new(resp);
            let mut stream = String::new();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "SSE stream never carried a response; events so far:\n{stream}"
                );
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => panic!("SSE stream ended without a response; events:\n{stream}"),
                    Ok(_) => {
                        stream.push_str(&line);
                        if let Some(data) = line.strip_prefix("data: ")
                            && let Ok(val) = serde_json::from_str::<serde_json::Value>(data.trim())
                            && val.get("id").is_some()
                        {
                            return (code, val);
                        }
                    }
                    Err(e) => panic!("read SSE stream: {e}"),
                }
            }
        }

        let text = resp.text().unwrap_or_default();
        let val = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        (code, val)
    }

    /// POST a client notification (no response body expected).
    fn notify(&mut self, body: &str) -> u16 {
        let resp = self.request(body).send().expect("http post");
        resp.status().as_u16()
    }
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

/// Open a session: initialize + the client's initialized notification.
fn open_session(url: String, token: Option<&str>) -> (HttpMcp, serde_json::Value) {
    let mut mcp = HttpMcp::new(url, token);
    let (code, resp) = mcp.call(INIT);
    assert_eq!(code, 200, "initialize must succeed: {resp:?}");
    let code = mcp.notify(INITIALIZED);
    assert!(
        code == 200 || code == 202,
        "initialized notification: {code}"
    );
    (mcp, resp)
}

#[test]
fn http_loopback_tokenless_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url, _logs) = spawn_http(dir.path(), None);

    // Tokenless + loopback bind is the allowed local-development mode.
    let (_, resp) = open_session(url, None);
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "agos-memory",
        "wire identity must be this crate: {resp:?}"
    );
    assert_eq!(
        resp["result"]["serverInfo"]["version"],
        agos_memory::VERSION,
        "{resp:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn http_remember_then_recall_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let token = "test-secret-token-1234";
    let (mut child, url, _logs) = spawn_http(dir.path(), Some(token));
    let (mut mcp, _) = open_session(url, Some(token));

    let remember = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"text":"The test agent prefers dark mode."}}}"#;
    let (code, resp) = mcp.call(remember);
    assert_eq!(code, 200, "remember should succeed: {resp:?}");
    assert!(
        resp["result"]["structuredContent"]["public_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "remember must return a public_id: {resp:?}"
    );

    // `remember` defaults to the episodic tier and episodic is opt-in for
    // recall (D26), so the recall must opt in.
    let recall = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall","arguments":{"text":"The test agent prefers dark mode.","include_episodic":true}}}"#;
    let (code, resp) = mcp.call(recall);
    assert_eq!(code, 200, "recall should succeed: {resp:?}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("dark mode"), "recall response: {resp:?}");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn http_missing_token_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let token = "test-secret-token-1234";
    let (mut child, url, _logs) = spawn_http(dir.path(), Some(token));

    let (code, resp) = HttpMcp::new(url, None).call(INIT);
    assert_eq!(
        code, 401,
        "missing token with configured token must be 401: {resp:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn http_wrong_token_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let token = "test-secret-token-1234";
    let (mut child, url, _logs) = spawn_http(dir.path(), Some(token));

    let (code, resp) = HttpMcp::new(url, Some("wrong-token-value")).call(INIT);
    assert_eq!(code, 401, "wrong token must be 401: {resp:?}");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn http_non_bearer_scheme_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let token = "test-secret-token-1234";
    let (mut child, url, _logs) = spawn_http(dir.path(), Some(token));

    // A non-Bearer scheme must not authenticate (RFC 9110 auth-scheme check).
    let (code, resp) = HttpMcp::new(url, Some("Basic dXNlcjpwYXNz")).call(INIT);
    assert_eq!(code, 401, "non-Bearer scheme must be 401: {resp:?}");

    let _ = child.kill();
    let _ = child.wait();
}
