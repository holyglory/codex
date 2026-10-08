//! Exercise the public resolver with child-only environment changes. The root
//! release gate runs these cases under UID 0 as well as the ordinary test user.
#![cfg(unix)]

use codex_utils_home_dir::find_codex_home;
use pretty_assertions::assert_eq;
use std::process::Command;

#[test]
fn inherited_home_and_explicit_codex_home() {
    const EXPECTED: &str = "CODEX_TEST_EXPECTED_HOME";
    if let Some(expected) = std::env::var_os(EXPECTED) {
        assert_eq!(
            find_codex_home().expect("resolve home").as_path(),
            std::path::Path::new(&expected)
        );
        return;
    }
    let temp = tempfile::tempdir().expect("isolated homes");
    let inherited = temp.path().join("inherited");
    let explicit = temp.path().join("explicit");
    std::fs::create_dir(&inherited).unwrap();
    std::fs::create_dir(&explicit).unwrap();
    let mut expected = inherited.canonicalize().unwrap().join(".codex");
    #[cfg(unix)]
    // SAFETY: geteuid has no arguments or failure mode.
    if unsafe { libc::geteuid() } == 0 {
        // Parent lookup ignores its environment too. The installed-package gate
        // independently binds passwd UID 0 to a known isolated home.
        assert!(
            std::env::var_os("CODEX_HOME").is_none(),
            "root test runner must unset CODEX_HOME"
        );
        expected = find_codex_home().expect("parent home").to_path_buf();
    }
    for (override_home, expected) in [
        (None, expected),
        (Some(&explicit), explicit.canonicalize().unwrap()),
    ] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "inherited_home_and_explicit_codex_home",
                "--nocapture",
            ])
            .env_remove("CODEX_HOME")
            .env("HOME", &inherited)
            .env(EXPECTED, expected);
        if let Some(path) = override_home {
            child.env("CODEX_HOME", path);
        }
        let output = child.output().expect("resolver child");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
