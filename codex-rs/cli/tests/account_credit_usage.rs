use std::path::Path;

use anyhow::Result;
use chrono::DateTime;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn account_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command.env("CODEX_HOME", codex_home).arg("account");
    Ok(command)
}

#[test]
fn credit_edits_preserve_saved_profiles_generations_and_cli_output() -> Result<()> {
    let codex_home = tempfile::tempdir()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    let mut account = AccountMetadata::new(
        "alpha".parse()?,
        AuthMode::Chatgpt,
        DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")?.into(),
    );
    account.id = "acct_00000000000000000000000000000001".parse()?;
    let store = RegistryStore::new(codex_home.path());
    let initial = AccountRegistry {
        default_account_id: Some(account.id.clone()),
        accounts: vec![account],
        ..AccountRegistry::default()
    };
    store.create(&initial)?;
    let legacy: Value = serde_json::from_slice(&std::fs::read(store.registry_path())?)?;
    assert_eq!(legacy["accounts"][0].get("creditUsageEnabled"), None);
    let mut view = json!({
        "id": "acct_00000000000000000000000000000001",
        "alias": "alpha",
        "authMode": "chatgpt",
        "email": null,
        "planType": null,
        "enabled": true,
        "authenticated": false,
        "priority": 1000,
        "creditUsageEnabled": false,
        "createdAt": "2026-08-20T00:00:00+00:00",
        "lastUsedAt": null,
        "note": null,
        "current": true,
    });
    let mut human_views = Vec::new();
    for (mode, generation, changed) in [
        ("enabled", 1, true),
        ("enabled", 1, false),
        ("disabled", 2, true),
    ] {
        view["creditUsageEnabled"] = json!(mode == "enabled");
        let output = account_command(codex_home.path())?
            .args(["edit", "alpha", "--credit-usage", mode, "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(
            serde_json::from_slice::<Value>(&output)?,
            json!({
                "schemaVersion": 1,
                "generation": generation,
                "action": "edit",
                "changed": changed,
                "account": view,
            })
        );
        let mut expected = initial.clone();
        expected.generation = generation;
        expected.accounts[0].credit_usage_enabled = mode == "enabled";
        assert_eq!(store.read()?, expected);
        if changed {
            let output = account_command(codex_home.path())?
                .arg("current")
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            human_views.push(String::from_utf8(output)?);
        }
    }
    insta::assert_snapshot!(human_views.join("\n"), @r"
    Account: alpha
    Status: enabled
    Authenticated: no
    Authentication: chatgpt
    Priority: 1000
    Credit use (automatic): enabled

    Account: alpha
    Status: enabled
    Authenticated: no
    Authentication: chatgpt
    Priority: 1000
    Credit use (automatic): disabled
    ");
    account_command(codex_home.path())?
        .args([
            "edit",
            "alpha",
            "--credit-usage",
            "enabled",
            "--expected-generation",
            "0",
        ])
        .assert()
        .failure();
    for mode in ["true", "ENABLED"] {
        account_command(codex_home.path())?
            .args(["edit", "alpha", "--credit-usage", mode])
            .assert()
            .failure();
    }
    account_command(codex_home.path())?
        .args(["edit", "alpha", "--credit-usage", "disabled"])
        .assert()
        .success()
        .stdout("Account alpha was already unchanged.\n");
    let shown = account_command(codex_home.path())?
        .args(["show", "alpha", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&shown)?,
        json!({
            "schemaVersion": 1,
            "generation": 2,
            "account": view,
        })
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(store.registry_path())?)?;
    assert_eq!(saved["accounts"][0].get("creditUsageEnabled"), None);
    assert_eq!(saved["generation"], 2);
    Ok(())
}

#[test]
fn account_tables_preserve_unicode_alignment_notes_and_command_order() -> Result<()> {
    let codex_home = tempfile::tempdir()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    let mut accounts = Vec::new();
    for (index, alias, priority, note, credit_usage_enabled) in [
        (1, "alpha", 1000, Some("東京 team"), true),
        (2, "beta", 10, None, false),
        (3, "gamma", 1000, Some("short"), false),
    ] {
        let mut account = AccountMetadata::new(
            alias.parse()?,
            AuthMode::Chatgpt,
            DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")?.into(),
        );
        account.id = format!("acct_{index:032x}").parse()?;
        account.priority = priority;
        account.note = note.map(str::to_string);
        account.credit_usage_enabled = credit_usage_enabled;
        accounts.push(account);
    }
    RegistryStore::new(codex_home.path()).create(&AccountRegistry {
        default_account_id: Some(accounts[0].id.clone()),
        accounts,
        ..AccountRegistry::default()
    })?;
    let output = account_command(codex_home.path())?
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    insta::assert_snapshot!(String::from_utf8(output)?, @r"
    ALIAS   PRIORITY  NOTE       CREDITS  CREDIT USE  BANKED RESETS  LIMITS                           RESET IN
     beta   10                   unknown  disabled    unknown        unknown (credentialUnavailable)  unknown
    *alpha  1000      東京 team  unknown  enabled     unknown        unknown (credentialUnavailable)  unknown
     gamma  1000      short      unknown  disabled    unknown        unknown (credentialUnavailable)  unknown
    ");
    let output = account_command(codex_home.path())?
        .args(["priority", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    insta::assert_snapshot!(String::from_utf8(output)?, @r"
    ALIAS   PRIORITY  NOTE
    *alpha  1000      東京 team
     gamma  1000      short
     beta   10
    ");
    for alias in ["alpha", "gamma"] {
        account_command(codex_home.path())?
            .args(["edit", alias, "--clear-note"])
            .assert()
            .success();
    }
    let output = account_command(codex_home.path())?
        .args(["priority", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    insta::assert_snapshot!(String::from_utf8(output)?, @r"
    ALIAS   PRIORITY
    *alpha  1000
     gamma  1000
     beta   10
    ");
    Ok(())
}
