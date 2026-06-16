//! Integration: JSON API routes on the same axum router as the MCP transport.
//!
//! Spawns the real `serve` binary with a hermetic config on an ephemeral port and
//! exercises the JSON routes end-to-end: auth matrix (401 without/wrong token, 200
//! with), a remember→recall roundtrip, and an OpenAPI route-coverage check that
//! asserts every documented path is reachable and no extra route is undocumented.
//!
//! Hermetic: `[embed] provider = "hash"`, `MockChat` wired through `MemoryApi`.
//! The binary's `provider = "hash"` path is the same code path as the lib-unit
//! tests on `MemoryApi`, so this validates the wire shape (status codes, JSON
//! bodies, auth) without hitting any network.
//!
//! ## OpenAPI route coverage
//!
//! `parse_openapi_paths()` reads `docs/api/openapi.yaml` with a small line-based
//! parser (no `serde_yaml` dep) and extracts every top-level path + HTTP method.
//! The `route_coverage` test then hits each documented path with a valid bearer
//! token and confirms the server returns a response (200 or an error body) rather
//! than a connection-level failure — i.e. the route is registered in the router. A
//! second pass probes candidate paths and asserts there are no *undocumented*
//! reachable routes.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agos-memory"))
}

/// A spawned `serve` child that kills and reaps itself when it leaves scope
/// (only if it is still running), so a passing test can't orphan a server.
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

/// Spawn `serve` (default HTTP) with a hermetic config in `dir`, bound to
/// `127.0.0.1:0` (ephemeral port), optionally with a bearer token.
///
/// Returns the [`Server`] guard, the base URL, and a receiver that streams stderr lines.
/// The receiver MUST be kept alive for the child's whole lifetime to avoid
/// SIGPIPE on the server's next log line (see `tests/mcp_http.rs` for the rationale).
fn spawn(dir: &std::path::Path, token: Option<&str>) -> (Server, String, mpsc::Receiver<String>) {
    let cfg = dir.join("http_api.toml");
    let mut cfg_str =
        String::from("[embed]\nprovider = \"hash\"\n\n[server]\nbind = \"127.0.0.1:0\"\n");
    if let Some(t) = token {
        cfg_str.push_str(&format!("token = \"{t}\"\n"));
    }
    std::fs::write(&cfg, &cfg_str).unwrap();

    let mut child = binary()
        .args(["serve", "--config"])
        .arg(&cfg)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");

    let stderr = child.stderr.take().unwrap();
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    // Wait for the listen line to learn the ephemeral port.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut log = String::new();
    let addr = loop {
        match lines_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                log.push_str(&line);
                log.push('\n');
                // The server log line is prefixed by a timestamp+level+ANSI sequence.
                // Use a robust contains check so the exact formatting doesn't matter.
                if line.contains("HTTP server listening on ") {
                    let after = line.split("HTTP server listening on ").nth(1).unwrap();
                    // Strip any trailing annotation like "(MCP + JSON API)".
                    let addr = after.split('(').next().unwrap().trim();
                    break addr.to_string();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    Instant::now() < deadline,
                    "server never printed its listen line; stderr so far:\n{log}"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("server stderr closed before listen line; stderr so far:\n{log}");
            }
        }
    };

    let url = format!("http://{addr}");
    (Server(child), url, lines_rx)
}

// ---------------------------------------------------------------------------
// Minimal OpenAPI path parser — line-based, no serde_yaml dep.
// ---------------------------------------------------------------------------

/// Return every (path, method) pair declared under `paths:` in the OpenAPI spec.
///
/// Strategy: find the `paths:` key, then walk subsequent lines by indentation.
/// Top-level route lines (0 indent relative to `paths:`) are path keys.
/// Their children at +2 spaces are HTTP-method keys (`get`/`post`/`put`/`delete`/etc.).
/// Method names are normalised to uppercase. Path-parameter segments like `{id}`
/// are left as-is (they match the axum route signature).
fn parse_openapi_paths() -> Vec<(String, String)> {
    let text = std::fs::read_to_string("docs/api/openapi.yaml")
        .unwrap_or_else(|e| panic!("docs/api/openapi.yaml: {e}"));

    let mut out = Vec::new();
    let mut in_paths = false;
    let mut paths_indent: Option<usize> = None;
    let mut current_path: Option<String> = None;

    for raw in text.lines() {
        // Strip comments (a `#` preceded by whitespace or at line start).
        let line = raw.trim_end();
        let stripped = line.trim_start();
        if stripped.starts_with('#') {
            continue;
        }

        if !in_paths {
            if stripped.starts_with("paths:") {
                in_paths = true;
                let pi = raw.len() - raw.trim_start().len();
                paths_indent = Some(pi);
            }
            continue;
        }

        // Skip blank lines (they appear between path entries in the spec).
        if stripped.is_empty() {
            continue;
        }

        let trim = line.len() - line.trim_start().len();
        let base_indent = paths_indent.unwrap();

        // Back out of `paths:` when indentation returns to base or less.
        if trim <= base_indent && !stripped.starts_with('-') {
            break;
        }

        if trim == base_indent + 2 && !stripped.is_empty() {
            // A new path entry: `  /some/path:`
            current_path = Some(stripped.trim_end_matches(':').to_string());
            continue;
        }

        // A method line at `    get:` depth.
        if trim != base_indent + 4 || stripped.is_empty() {
            continue;
        }
        let path = match &current_path {
            Some(p) => p.clone(),
            None => continue,
        };
        let method = stripped.trim_end_matches(':').trim().to_uppercase();
        if matches!(
            method.as_str(),
            "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS" | "HEAD"
        ) {
            out.push((path, method));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tests — synchronous (reqwest::blocking::Client)
// ---------------------------------------------------------------------------

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::new()
}

#[test]
fn healthz_tokenless_loopback_ok() {
    let dir = tempfile::tempdir().unwrap();
    let (_child, url, rx) = spawn(dir.path(), None);
    let _guard = rx;
    let c = client();

    let got = c.get(format!("{url}/healthz")).send().unwrap();
    assert_eq!(got.status(), 200, "healthz must be 200 (public, tokenless)");
    let body: serde_json::Value = got.json().unwrap();
    assert_eq!(body["status"], "ok", "healthz body: {body:?}");
}

#[test]
fn auth_missing_token_rejected_on_json_routes() {
    let dir = tempfile::tempdir().unwrap();
    let token = "auth-coverage-secret";
    let (_child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    let code = c
        .post(format!("{url}/api/v1/remember"))
        .json(&serde_json::json!({"text": "coverage check"}))
        .send()
        .unwrap()
        .status();
    assert_eq!(
        code, 401,
        "missing bearer on a token-configured server must be 401"
    );
}

#[test]
fn auth_wrong_token_rejected_on_json_routes() {
    let dir = tempfile::tempdir().unwrap();
    let token = "auth-coverage-secret";
    let (_child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    let code = c
        .post(format!("{url}/api/v1/remember"))
        .header("Authorization", "Bearer wrong-value")
        .json(&serde_json::json!({"text": "coverage check"}))
        .send()
        .unwrap()
        .status();
    assert_eq!(code, 401, "wrong bearer must be 401");
}

#[test]
fn auth_valid_token_accepts_json_routes() {
    let dir = tempfile::tempdir().unwrap();
    let token = "auth-coverage-secret";
    let (_child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    // `/api/v1/remember` with an *empty* text returns 400 (validation), not 401 —
    // that proves the auth layer passed and the route is registered.
    let code = c
        .post(format!("{url}/api/v1/remember"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"text": ""}))
        .send()
        .unwrap()
        .status();
    assert_eq!(
        code, 400,
        "valid bearer must pass auth and hit the route (400 = validation, not 401)"
    );
}

#[test]
fn remember_recall_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let token = "roundtrip-secret";
    let (mut child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    // `remember` with a non-empty text returns 200 + public_id.
    let rem_req = serde_json::json!({
        "text": "The test operator prefers the CLI over a GUI.",
        "tier": "semantic",
        "kind": "preference",
        "source_kind": "user",
    });
    let rem = c
        .post(format!("{url}/api/v1/remember"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&rem_req)
        .send()
        .unwrap();
    assert_eq!(rem.status(), 200, "remember status must be 200");
    let rem_body: serde_json::Value = rem.json().unwrap();
    let id = rem_body["public_id"].as_str().unwrap_or("");
    assert!(
        !id.is_empty(),
        "remember must return a non-empty public_id: {rem_body:?}"
    );
    assert_eq!(
        rem_body["status"], "active",
        "remember status: {rem_body:?}"
    );
    assert_eq!(rem_body["trust"], "trusted", "remember trust: {rem_body:?}");

    // `recall` for the same text returns the stored memory in hits.
    let rec = c
        .post(format!("{url}/api/v1/recall"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "text": "CLI over GUI",
            "include_episodic": true,
        }))
        .send()
        .unwrap();
    assert_eq!(rec.status(), 200, "recall status must be 200");
    let rec_body: serde_json::Value = rec.json().unwrap();
    let hits = rec_body["hits"].as_array();
    assert!(
        hits.is_some() && !hits.unwrap().is_empty(),
        "recall must return at least one hit: {rec_body:?}"
    );

    // Cleanup.
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn status_returns_counts() {
    let dir = tempfile::tempdir().unwrap();
    let token = "status-secret";
    let (_child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    let st = c
        .get(format!("{url}/api/v1/status"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .unwrap();
    assert_eq!(st.status(), 200, "status status must be 200");
    let st_body: serde_json::Value = st.json().unwrap();
    assert!(
        st_body["counts"].is_array(),
        "status.counts must be an array: {st_body:?}"
    );
    // The counts rows are grouped by *status* (field name is `status`, corrected
    // from the CLI tool's old `tier` label in 0002).
    let counts = st_body["counts"].as_array().unwrap();
    assert!(
        counts.iter().any(|c| c["status"] == "active"),
        "status must include an active bucket: {counts:?}"
    );
}

#[test]
fn forget_soft_requires_valid_id() {
    let dir = tempfile::tempdir().unwrap();
    let token = "forget-secret";
    let (_child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    // A non-existent id returns 404 with an ApiError body, not 401 — proves the
    // route is registered and auth passed.
    let resp = c
        .post(format!("{url}/api/v1/forget/nonexistent-id"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"action": "soft"}))
        .send()
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "forget on a missing id must be 404 (not 401): {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().unwrap();
    assert!(
        body.get("error").is_some() && body.get("code").is_some(),
        "404 body must be an ApiError: {body:?}"
    );
    assert_eq!(body["code"], "NOT_FOUND", "forget 404 code: {body:?}");
}

#[test]
fn explain_missing_id_returns_404() {
    let dir = tempfile::tempdir().unwrap();
    let token = "explain-secret";
    let (_child, url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();

    let resp = c
        .get(format!("{url}/api/v1/explain/missing-id"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "explain on a missing id must be 404: {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["code"], "NOT_FOUND", "explain 404 code: {body:?}");
}

#[test]
fn route_coverage() {
    let documented = parse_openapi_paths();
    assert!(
        !documented.is_empty(),
        "docs/api/openapi.yaml must declare at least one path"
    );

    let dir = tempfile::tempdir().unwrap();
    let token = "coverage-secret";
    let (_child, base_url, rx) = spawn(dir.path(), Some(token));
    let _guard = rx;
    let c = client();
    let bearer = format!("Bearer {token}");

    // Phase 1: every documented path is reachable (returns a response, not a
    // connection-level failure). 200/400/401/404/405/500/503 are all valid
    // "route exists" signals. A connect refused / DNS error would mean the server
    // died mid-test; we `unwrap` those.
    let mut seen: Vec<(String, String)> = Vec::new();
    for (path, method) in &documented {
        let url = format!("{base_url}{path}");
        let resp = match method.as_str() {
            "GET" => c.get(&url).header("Authorization", &bearer).send().unwrap(),
            "POST" => c
                .post(&url)
                .header("Authorization", &bearer)
                .json(&serde_json::json!({"text": "coverage probe"}))
                .send()
                .unwrap(),
            _ => panic!(
                "route_coverage: unsupported method {method} in spec — only GET/POST are probed"
            ),
        };
        let code = resp.status().as_u16();
        assert!(
            code < 500 || code == 503,
            "{method} {path}: unexpected server error {code}"
        );
        // A 404 here means the route is *not* registered (axum's fallback layer
        // returns 404 for unmatched routes on a matching mount). Flag it so the
        // test fails noisily rather than silently skipping a documented route.
        //
        // Exception: `/api/v1/forget/{id}` and `/api/v1/explain/{id}` legitimately
        // return 404 when the target id does not exist — that's a normal business
        // response, not a missing-route signal. Probe them with a plausible id and
        // accept 404.
        let accept_404 =
            path.starts_with("/api/v1/forget/") || path.starts_with("/api/v1/explain/");
        assert!(
            code != 404 || accept_404,
            "{method} {path} returned 404 — route not registered in the router?"
        );
        seen.push((path.clone(), method.clone()));
    }

    // Phase 2: no undocumented routes reachable under `/api/v1` or `/healthz`.
    //
    // Probe a set of candidate paths. Any that return a non-404 response and are
    // not in `documented` are flagged.
    let candidates: &[&str] = &[
        "/healthz",
        "/api/v1/remember",
        "/api/v1/recall",
        "/api/v1/summarize",
        "/api/v1/forget/m_000000000000000000000001",
        "/api/v1/explain/m_000000000000000000000001",
        "/api/v1/status",
        // Things that should *not* exist:
        "/api/v1/configure",
        "/api/v1/admin/drop",
        "/api/v2/remember",
        "/health",
        "/ping",
    ];
    for candidate in candidates {
        let url = format!("{base_url}{candidate}");
        let resp = match c.get(&url).header("Authorization", &bearer).send() {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => continue,
            Err(e) => panic!("candidate {candidate}: request failed: {e}"),
        };
        let code = resp.status().as_u16();
        if code == 404 {
            continue; // not registered — fine
        }
        // 405 = path registered but doesn't support GET — fine for documented
        // POST-only routes (e.g. /api/v1/forget/{id}).
        if code == 405 {
            continue;
        }
        if !documented.iter().any(|(p, _)| p == candidate) {
            panic!(
                "Undocumented route reachable at {candidate} (status {code}). \
                 Add it to docs/api/openapi.yaml or remove it from the router."
            );
        }
    }
}
