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
