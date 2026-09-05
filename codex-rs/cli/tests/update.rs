use anyhow::Result;
use predicates::str::contains;
use std::path::Path;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn update_does_not_start_interactive_prompt() -> Result<()> {
    let codex_home = TempDir::new()?;

    codex_command(codex_home.path())?
        .arg("update")
        .assert()
        .failure()
        .stderr(contains("`codex update` is not available in debug builds"));

    Ok(())
}

#[cfg(not(debug_assertions))]
#[test]
fn npm_update_executes_the_fork_command_and_reports_installer_failure() -> Result<()> {
    use pretty_assertions::assert_eq;

    let codex_home = TempDir::new()?;
    let shims = TempDir::new()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let npm = shims.path().join("npm");
        std::fs::write(
            &npm,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CODEX_UPDATE_ARGS\"\nexit \"$CODEX_UPDATE_EXIT\"\n",
        )?;
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(/*mode*/ 0o700))?;
    }
    #[cfg(windows)]
    std::fs::write(
        shims.path().join("npm.cmd"),
        "@echo off\r\n(echo %1& echo %2& echo %3)>\"%CODEX_UPDATE_ARGS%\"\r\nexit /b %CODEX_UPDATE_EXIT%\r\n",
    )?;

    for exit_code in [0, 42] {
        let receipt = shims.path().join(format!("arguments-{exit_code}"));
        let assertion = codex_command(codex_home.path())?
            .arg("update")
            .env("PATH", shims.path())
            .env("CODEX_MANAGED_BY_NPM", "1")
            .env_remove("CODEX_MANAGED_BY_VITE_PLUS")
            .env_remove("CODEX_MANAGED_BY_PNPM")
            .env_remove("CODEX_MANAGED_BY_BUN")
            .env("CODEX_UPDATE_ARGS", &receipt)
            .env("CODEX_UPDATE_EXIT", exit_code.to_string())
            .assert();
        if exit_code == 0 {
            assertion
                .success()
                .stdout(contains("Update ran successfully"));
        } else {
            assertion.failure().stderr(contains("failed with status"));
        }
        assert_eq!(
            std::fs::read_to_string(receipt)?
                .lines()
                .collect::<Vec<_>>(),
            vec!["install", "-g", "@holyglory/codex@latest"]
        );
    }
    Ok(())
}
