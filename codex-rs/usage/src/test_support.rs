//! Fresh-process fixtures for assertions about bounded process-local cache identity.

use std::env;
use std::process::Command;

/// Runs the named case in a child. Only the child receives the marker to print
/// after its assertions have passed, so a filter matching zero tests cannot succeed.
pub(crate) fn enter_isolated_test(case: &str) -> Option<String> {
    const CHILD_CASE: &str = "CODEX_USAGE_ISOLATED_TEST_CASE";
    let marker = format!("codex-usage-isolated-test-all-assertions-passed:{case}");
    if env::var(CHILD_CASE).as_deref() == Ok(case) {
        return Some(marker);
    }
    let output = Command::new(env::current_exe().expect("test executable"))
        .args(["--exact", case, "--nocapture"])
        .env(CHILD_CASE, case)
        .output()
        .expect("isolated usage test case");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&marker),
        "{case} failed or did not run in an isolated process: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    None
}
