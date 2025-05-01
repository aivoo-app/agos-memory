//! Integration: the plan-guard CI gate passes on this repository.
//! (plan/ is kept untracked by its own `plan/.gitignore` containing `*`;
//! this only asserts the guard script agrees.)

use std::process::Command;

#[test]
fn plan_guard_passes_when_nothing_local_is_tracked() {
    let out = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/ci/check-plan-not-tracked.sh"
        ))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run plan-guard script");
    assert!(
        out.status.success(),
        "plan-guard failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
