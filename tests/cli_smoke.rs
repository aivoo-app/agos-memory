//! Integration: CLI smoke tests driving the real binary
//! (`init` / `status` / `doctor`) in a scratch directory. No new
//! dev-dependencies — `CARGO_BIN_EXE_*` is provided by cargo itself.

use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agos-memory"))
}

fn run(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = binary()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn agos-memory");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Spawn with `payload` fed to the child's stdin (then closed → EOF) and read
/// its stdout/stderr. Used for the `import -` pipe contract.
fn run_pipe(dir: &std::path::Path, args: &[&str], payload: &str) -> (String, String) {
    let mut child = binary()
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agos-memory");
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(payload.as_bytes()).expect("write payload");
    stdin.flush().expect("flush payload");
    drop(stdin); // EOF so `import -` sees the end of its input

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut out = String::new();
    let mut err = String::new();
    BufReader::new(stdout)
        .read_to_string(&mut out)
        .expect("read stdout");
    BufReader::new(stderr)
        .read_to_string(&mut err)
        .expect("read stderr");
    let _ = child.wait();
    (out, err)
}

#[test]
fn init_then_status_then_doctor_succeed() {
    let dir = tempfile::tempdir().unwrap();

    let (code, stdout, _) = run(dir.path(), &["init", "--force"]);
    assert_eq!(code, 0, "init failed: {stdout}");
    assert!(stdout.contains("init complete"), "{stdout}");
    assert!(stdout.contains("schema:"), "{stdout}");
    assert!(stdout.contains("sqlite-vec:"), "{stdout}");

    // `init` wrote the scaffold into the scratch cwd.
    assert!(dir.path().join("agos-memory.toml").is_file());

    let (code, stdout, _) = run(dir.path(), &["status"]);
    assert_eq!(code, 0, "status failed: {stdout}");
    assert!(stdout.contains("memories:"), "{stdout}");

    let (code, stdout, _) = run(dir.path(), &["doctor"]);
    assert_eq!(code, 0, "doctor failed: {stdout}");
    assert!(stdout.contains("doctor done"), "{stdout}");
    assert!(stdout.contains("integrity: ok"), "{stdout}");
    assert!(stdout.contains("foreign_keys = 1"), "{stdout}");
}

#[test]
fn backup_writes_a_verified_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "--force"]);

    let (code, stdout, stderr) = run(dir.path(), &["backup", "--out", "snap.db"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("backup complete"), "{stdout}");
    assert!(stdout.contains("integrity:   ok"), "{stdout}");
    assert!(dir.path().join("snap.db").is_file());

    // A second run must refuse to overwrite the snapshot file (exit 2).
    let (code, _, stderr) = run(dir.path(), &["backup", "--out", "snap.db"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
}

#[test]
fn init_is_idempotent_without_force() {
    let dir = tempfile::tempdir().unwrap();

    let (code, stdout, _) = run(dir.path(), &["init"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("wrote agos-memory.toml"), "{stdout}");

    let (code, stdout, _) = run(dir.path(), &["init"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("already exists"),
        "re-init without --force must not overwrite: {stdout}"
    );
}

#[test]
fn doctor_fails_with_a_clear_message_when_the_database_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run(dir.path(), &["doctor"]);
    assert_ne!(code, 0, "doctor must fail on a missing database");
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("run `agos-memory init`"), "{stdout}");
    assert!(!stderr.is_empty(), "stderr should carry the error line");
}

#[test]
fn remember_writes_a_memory_offline_and_it_survives_the_process() {
    let dir = tempfile::tempdir().unwrap();
    // Offline config: degraded keyword-only mode, no provider needed (D4).
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n\n[llm]\nbase_url = ''\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run(dir.path(), &["session", "open"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("session:"), "{stdout}");

    let (code, stdout, stderr) = run(
        dir.path(),
        &[
            "session",
            "append",
            "--role",
            "user",
            "--content",
            "I prefer Rust for CLI tools.",
        ],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.contains("turn:"), "{stdout}");

    let (code, stdout, stderr) = run(dir.path(), &["remember", "--text", "Prefers Rust."]);
    assert_eq!(
        code, 0,
        "offline remember must not need a provider: {stdout}{stderr}"
    );
    assert!(stdout.contains("memory:"), "{stdout}");

    // A *new process* opens the same database: the fact is durable.
    let (code, stdout, stderr) = run(dir.path(), &["status"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("memories[active]: 1"),
        "fact must survive the process: {stdout}"
    );
}

#[test]
fn remember_keeps_the_fact_when_the_embedding_provider_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    // Port 1 refuses immediately: the provider is unreachable, not slow.
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n\
         [embed]\nprovider = 'openai_compat'\nbase_url = 'http://127.0.0.1:1'\n\
         model = 'text-embedding-3-small'\ntimeout_secs = 2\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run(dir.path(), &["remember", "--text", "Provider is down."]);
    assert_eq!(
        code, 0,
        "a provider outage must never lose a fact: {stdout}{stderr}"
    );
    assert!(stdout.contains("memory:"), "{stdout}");

    let (code, stdout, stderr) = run(dir.path(), &["status"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("memories[active]: 1"),
        "unembedded fact must still be stored: {stdout}"
    );
}

#[test]
fn forget_rollback_errors_on_unknown_memory() {
    // 0056: `forget rollback` must be a wired subcommand that fails loudly
    // (proper exit code + message) when the target memory does not exist. The
    // happy path (revert text, intact version chain) is covered by the data
    // layer in tests/versioning.rs, which exercises the same rollback_memory
    // the CLI dispatches to.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n\n[llm]\nbase_url = ''\n",
    )
    .unwrap();
    run(dir.path(), &["init", "--force"]);

    let (code, stdout, stderr) = run(
        dir.path(),
        &["forget", "rollback", "nope", "--to-version", "1"],
    );
    assert_ne!(
        code, 0,
        "rollback of a missing memory must fail: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("not found") || stderr.contains("not found"),
        "error must name the missing memory: {stdout}{stderr}"
    );
}

#[test]
fn forget_rollback_rejects_invalid_version() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n\n[llm]\nbase_url = ''\n",
    )
    .unwrap();
    run(dir.path(), &["init", "--force"]);

    let (code, _stdout, stderr) = run(
        dir.path(),
        &["forget", "rollback", "nope", "--to-version", "0"],
    );
    assert_ne!(code, 0, "to-version 0 must be rejected: {stderr}");
}

#[test]
fn summarize_offline_runs_on_an_initted_database() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n\n[llm]\nbase_url = ''\n",
    )
    .unwrap();
    run(dir.path(), &["init", "--force"]);

    // Create a memory to summarize, then extract its public_id from output
    let (code, stdout, _) = run(
        dir.path(),
        &[
            "remember",
            "--text",
            "Rust is a systems programming language designed for safety",
        ],
    );
    assert_eq!(code, 0, "remember must succeed: {stdout}");

    // Extract public_id from "memory:  <uuid> (active)" line
    let pid = stdout
        .lines()
        .find(|l| l.starts_with("memory:"))
        .and_then(|l| {
            // Format: "memory:  <uuid> (active)"
            l.split_whitespace().nth(1).map(|w| w.to_string())
        })
        .expect("remember output must contain public_id");

    // summarize --id should run (exit 0) on offline DB with MockChat
    // Since we use provider='none', it falls back to MockChat
    let (code, _stdout, stderr) = run(dir.path(), &["summarize", "--id", &pid]);
    assert_eq!(
        code, 0,
        "summarize --id must succeed offline (MockChat fallback): {stderr}"
    );
}

#[test]
fn export_import_roundtrip_across_a_fresh_database() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n\n[llm]\nbase_url = ''\n",
    )
    .unwrap();
    // Bootstrap the DB without `init --force` (which would overwrite the
    // provider='none' config with the openai default): `session open` creates
    // the store on first use.
    run(dir.path(), &["session", "open"]);

    let (code, stdout, _) = run(
        dir.path(),
        &["remember", "--text", "pipe contract memory for export"],
    );
    assert_eq!(code, 0, "remember failed: {stdout}");

    // export --out writes the JSONL file (header + one row) and exits 0. The path
    // is cwd-relative (run() sets current_dir), so a plain literal works.
    let (code, stdout, stderr) = run(dir.path(), &["export", "--out", "out.jsonl"]);
    assert_eq!(code, 0, "export failed: {stdout}{stderr}");
    let raw = std::fs::read_to_string(dir.path().join("out.jsonl")).expect("export file readable");
    let lines: Vec<&str> = raw.lines().collect();
    assert!(lines.len() >= 2, "header + row expected: {raw}");
    assert!(lines[0].contains("agos-memory-export"), "header: {raw}");

    // import into a fresh database (--db) reports inserted rows.
    let (code, stdout, stderr) = run(dir.path(), &["--db", "dst.db", "import", "out.jsonl"]);
    assert_eq!(code, 0, "import failed: {stdout}{stderr}");
    assert!(stdout.contains("inserted=1"), "{stdout}");

    // The imported database is readable and counts one active memory.
    let (code, stdout, _) = run(dir.path(), &["--db", "dst.db", "status"]);
    assert_eq!(code, 0, "status on imported db failed");
    assert!(stdout.contains("memories[active]: 1"), "{stdout}");
}

#[test]
fn import_accepts_jsonl_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agos-memory.toml"),
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n\n[llm]\nbase_url = ''\n",
    )
    .unwrap();
    // Bootstrap the DB without init (init --force would overwrite provider).
    run(dir.path(), &["session", "open"]);
    run(
        dir.path(),
        &["remember", "--text", "stdin pipe contract memory"],
    );

    // Stream export to stdout and pipe it straight back in through `import -`.
    let (jsonl, _) = run_pipe(dir.path(), &["export"], "");
    assert!(
        jsonl.contains("agos-memory-export"),
        "export to stdout: {jsonl}"
    );

    let (out, err) = run_pipe(dir.path(), &["--db", "pipe.db", "import", "-"], &jsonl);
    assert!(out.contains("inserted=1"), "import - summary: {out}{err}");

    let (code, out, _) = run(dir.path(), &["--db", "pipe.db", "status"]);
    assert_eq!(code, 0);
    assert!(out.contains("memories[active]: 1"), "{out}");
}

#[test]
fn status_reports_a_missing_database_and_exit_codes_stay_stable() {
    let dir = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(dir.path(), &["status"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("no database at"), "{stdout}");

    // A bad --db path is a storage error (exit 5) for doctor, per the
    // documented exit-code table.
    let (code, _, _) = run(dir.path(), &["--db", "/nonexistent/dir/x.db", "doctor"]);
    assert_ne!(code, 0);
}
