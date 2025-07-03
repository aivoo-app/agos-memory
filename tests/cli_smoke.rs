//! Integration: CLI smoke tests driving the real binary
//! (`init` / `status` / `doctor`) in a scratch directory. No new
//! dev-dependencies — `CARGO_BIN_EXE_*` is provided by cargo itself.

use std::process::Command;

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
        "db_path = 'smoke.db'\nagent_id = 'default'\n\n[embed]\nprovider = 'none'\n",
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
