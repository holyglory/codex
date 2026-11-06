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
    let view = json!({
        "id": "acct_00000000000000000000000000000001",
        "alias": "alpha",
        "authMode": "chatgpt",
        "email": null,
        "planType": null,
        "enabled": true,
        "authenticated": false,
        "priority": 1000,
        "createdAt": "2026-08-20T00:00:00+00:00",
        "lastUsedAt": null,
        "note": null,
        "current": true,
    });
    for (mode, generation, changed) in [
        ("enabled", 1, true),
        ("enabled", 1, false),
        ("disabled", 2, true),
    ] {
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
    }
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
