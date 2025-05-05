//! Integration: the plan-guard CI gate passes on this repository.
//! (plan/ is kept untracked by its own `plan/.gitignore` containing `*`;
//! this asserts nothing under plan/ except that file is tracked.)

use std::process::Command;

#[test]
fn plan_guard_passes_when_nothing_local_is_tracked() {
    let out = Command::new("git")
        .args(["ls-files", "plan/"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    let tracked: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap_or_default()
        .lines()
        .filter(|l| *l != "plan/.gitignore")
        .collect();
    assert!(
        tracked.is_empty(),
        "plan-guard failed — tracked under plan/: {tracked:?}"
    );
}
