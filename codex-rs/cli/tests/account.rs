use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;

use anyhow::Result;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::encode_id_token;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::OpaqueServiceId;
use codex_account_registry::RegistryStore;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ProfileAuthStorage;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home)
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("OPENAI_API_KEY");
    Ok(command)
}

fn auth() -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("fixture-secret".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn chatgpt_auth(alias: &str) -> Result<AuthDotJson> {
    let token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email(format!("{alias}@example.test"))
            .plan_type("pro")
            .chatgpt_user_id(format!("user-{alias}"))
            .chatgpt_account_id(format!("workspace-{alias}")),
    )?;
    Ok(AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&token)?,
            access_token: format!("access-{alias}"),
            refresh_token: format!("refresh-{alias}"),
            account_id: Some(format!("workspace-{alias}")),
        }),
        last_refresh: Some(chrono::Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    })
}

struct Fixture {
    home: TempDir,
    alpha: AccountMetadata,
    beta: AccountMetadata,
}

fn fixture(beta_authenticated: bool) -> Result<Fixture> {
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    let mut alpha = AccountMetadata::new(
        "alpha".parse::<AccountAlias>()?,
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    alpha.priority = 1;
    alpha.note = Some("primary".to_string());
    alpha.service_account_id = Some(OpaqueServiceId::new("protected-account-id")?);
    alpha.service_workspace_id = Some(OpaqueServiceId::new("protected-workspace-id")?);
    let mut beta = AccountMetadata::new(
        "beta".parse::<AccountAlias>()?,
        AuthMode::ApiKey,
        chrono::Utc::now(),
    );
    beta.priority = 2;
    for account in [&alpha, &beta] {
        let profile = ProfileAuthStorage::new(
            home.path(),
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?;
        if account.id == alpha.id || beta_authenticated {
            profile.save(&auth())?;
        }
    }
    RegistryStore::new(home.path()).create(&AccountRegistry {
        default_account_id: Some(alpha.id.clone()),
        accounts: vec![alpha.clone(), beta.clone()],
        ..AccountRegistry::default()
    })?;
    Ok(Fixture { home, alpha, beta })
}

fn stdout_json(assertion: assert_cmd::assert::Assert) -> Result<Value> {
    Ok(serde_json::from_slice(&assertion.get_output().stdout)?)
}

#[test]
fn help_exposes_only_fully_implemented_account_surfaces() -> Result<()> {
    let home = TempDir::new()?;
    let output = codex_command(home.path())?
        .args(["account", "--help"])
        .output()?;
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout)?;
    for command in [
        "list", "current", "show", "add", "limits", "reset", "rename", "edit", "priority", "use",
        "enable", "disable", "remove", "auto", "doctor",
    ] {
        assert!(help.contains(command), "missing account command {command}");
    }
    let root_help = codex_command(home.path())?.arg("--help").output()?;
    assert!(String::from_utf8(root_help.stdout)?.contains("--account <ALIAS_OR_ID>"));
    Ok(())
}

#[test]
fn list_current_and_show_json_are_versioned_and_redacted() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let list = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "list", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(list["schemaVersion"], 1);
    assert_eq!(list["activeAccount"], "alpha");
    assert_eq!(list["priorityOrder"], "higherFirst");
    assert_eq!(list["accounts"].as_array().map(Vec::len), Some(2));
    let encoded = serde_json::to_string(&list)?;
    for prohibited in [
        "fixture-secret",
        "protected-account-id",
        "protected-workspace-id",
        fixture.home.path().to_string_lossy().as_ref(),
    ] {
        assert!(!encoded.contains(prohibited));
    }

    let current = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "current", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(current["account"]["alias"], "alpha");
    let shown = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "show", "beta", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(shown["account"]["alias"], "beta");
    Ok(())
}

#[tokio::test]
async fn account_list_aligns_columns_after_long_alias() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let long_alias = "axel-marchenko-tt-eu-pp".parse::<AccountAlias>()?;
    RegistryStore::new(fixture.home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            registry.accounts[1].alias = long_alias.clone();
        },
    )?;

    let output = codex_command(fixture.home.path())?
        .args(["account", "list"])
        .assert()
        .success();
    let human = String::from_utf8(output.get_output().stdout.clone())?;
    let mut lines = human.lines();
    lines.next().expect("account list header");
    let first_account = lines.next().expect("first account");
    let second_account = lines.next().expect("second account");
    let first_columns = first_account.split('\t').collect::<Vec<_>>();
    let second_columns = second_account.split('\t').collect::<Vec<_>>();
    assert_eq!(first_columns[2], "ready");
    assert_eq!(second_columns[2], "ready");
    assert_eq!(first_columns[1].len(), second_columns[1].len());
    insta::assert_snapshot!("account_list_long_alias", human);
    assert!(!human.contains('\x1b'));
    let program = codex_utils_cargo_bin::cargo_bin("codex")?;
    let ansi = regex_lite::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]")?;
    for no_color in [false, true] {
        let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
        for key in [
            "CODEX_API_KEY",
            "CODEX_ACCESS_TOKEN",
            "OPENAI_API_KEY",
            "NO_COLOR",
            "CLICOLOR",
            "CLICOLOR_FORCE",
            "FORCE_COLOR",
        ] {
            env.remove(key);
        }
        env.insert(
            "CODEX_HOME".to_string(),
            fixture.home.path().to_string_lossy().into_owned(),
        );
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        if no_color {
            env.insert("NO_COLOR".to_string(), "1".to_string());
        }
        let spawned = codex_utils_pty::spawn_pty_process(
            program.to_str().unwrap(),
            &["account".to_string(), "list".to_string()],
            fixture.home.path(),
            &env,
            /*arg0*/ &None,
            codex_utils_pty::TerminalSize {
                rows: 24,
                cols: 240,
            },
            &[],
        )
        .await?;
        let session = spawned.session;
        let mut stdout_rx = spawned.stdout_rx;
        let (code, output) =
            tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 30), async {
                let mut output = Vec::new();
                while let Some(bytes) = stdout_rx.recv().await {
                    output.extend(bytes);
                }
                Ok::<_, anyhow::Error>((spawned.exit_rx.await?, String::from_utf8(output)?))
            })
            .await??;
        drop(session);
        assert_eq!(code, 0);
        assert_eq!(output.contains('\x1b'), !no_color);
        assert_eq!(ansi.replace_all(&output, "").replace('\r', ""), human);
        if !no_color {
            insta::assert_snapshot!(
                "account_list_colors",
                output.replace('\x1b', "ESC").replace('\r', "")
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_reset_selects_soonest_credit_and_refreshes_account() -> Result<()> {
    let server = MockServer::start().await;
    let fixture = fixture(/*beta_authenticated*/ true)?;
    std::fs::write(
        fixture.home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}\"\n",
            server.uri()
        ),
    )?;
    ProfileAuthStorage::new(
        fixture.home.path(),
        fixture.alpha.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&chatgpt_auth("alpha")?)?;
    RegistryStore::new(fixture.home.path())
        .compare_and_swap(/*expected_generation*/ 0, |registry| {
            registry.accounts[0].auth_mode = AuthMode::Chatgpt
        })?;
    let credits = serde_json::json!({
        "available_count": 2,
        "credits": [
            {"id": "credit-later", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-01T00:00:00Z", "expires_at": "2030-01-01T00:00:00Z"},
            {"id": "credit-soon", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-02T00:00:00Z", "expires_at": "2027-01-01T00:00:00Z"}
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/codex/rate-limit-reset-credits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(credits.clone()))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {"allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 10, "limit_window_seconds": 300,
                    "reset_after_seconds": 0, "reset_at": 1893456000}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "reset", "windows_reset": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    let output = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "reset", "alpha", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(output["account"], "alpha");
    assert_eq!(output["creditId"], "credit-soon");
    assert_eq!(output["outcome"], "reset");
    assert_eq!(output["windowsReset"], 2);
    assert_eq!(output["refreshedBankedResets"]["availableCount"], 2);
    assert!(
        output["requestId"]
            .as_str()
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
    );
    server.verify().await;
    Ok(())
}

#[test]
fn metadata_activation_enable_disable_and_auto_commands_persist() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    codex_command(fixture.home.path())?
        .args(["account", "rename", "beta", "gamma"])
        .assert()
        .success();
    codex_command(fixture.home.path())?
        .args([
            "account",
            "edit",
            "gamma",
            "--priority",
            "0",
            "--note",
            "secondary",
        ])
        .assert()
        .success();
    codex_command(fixture.home.path())?
        .args(["account", "use", "gamma"])
        .assert()
        .success();
    codex_command(fixture.home.path())?
        .args(["account", "disable", "gamma"])
        .assert()
        .success();
    codex_command(fixture.home.path())?
        .args(["account", "enable", "gamma"])
        .assert()
        .success();
    let auto = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "auto", "on", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(auto["enabled"], true);
    codex_command(fixture.home.path())?
        .args(["account", "auto", "status"])
        .assert()
        .success()
        .stdout(contains(
            "eligible: locally managed ChatGPT OAuth profiles only",
        ));
    let registry = RegistryStore::new(fixture.home.path()).read()?;
    let gamma = registry.lookup("gamma")?;
    assert_eq!(
        (gamma.priority, gamma.note.as_deref()),
        (0, Some("secondary"))
    );
    assert_eq!(registry.default_account_id, Some(fixture.alpha.id));
    assert!(registry.auto_selection.enabled);
    Ok(())
}

#[test]
fn explicit_priority_commands_are_atomic_and_idempotent() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let listed = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "priority", "list", "--json"])
            .assert()
            .success(),
    )?;
    let aliases = listed["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|account| account["alias"].as_str().expect("alias"))
        .collect::<Vec<_>>();
    assert_eq!(listed["priorityOrder"], "higherFirst");
    assert_eq!(aliases, vec!["beta", "alpha"]);

    let set = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "priority", "set", "alpha", "3", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(set["account"]["priority"], 3);
    assert_eq!(set["changed"], true);

    let before_all = RegistryStore::new(fixture.home.path()).read()?.generation;
    let before_all = before_all.to_string();
    let normalized = stdout_json(
        codex_command(fixture.home.path())?
            .args([
                "account",
                "priority",
                "set-all",
                "1000",
                "--expected-generation",
                &before_all,
                "--json",
            ])
            .assert()
            .success(),
    )?;
    assert_eq!(normalized["changed"], true);
    assert_eq!(normalized["changedCount"], 2);
    assert_eq!(normalized["accounts"], serde_json::json!(["alpha", "beta"]));
    let normalized_generation = normalized["generation"].as_u64().expect("generation");
    let registry = RegistryStore::new(fixture.home.path()).read()?;
    assert!(
        registry
            .accounts
            .iter()
            .all(|account| account.priority == 1000)
    );
    assert_eq!(registry.generation, normalized_generation);

    let unchanged = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "priority", "set-all", "1000", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(unchanged["changed"], false);
    assert_eq!(unchanged["changedCount"], 0);
    assert_eq!(unchanged["generation"], normalized_generation);
    Ok(())
}

#[test]
fn auto_on_initializes_an_empty_registry() -> Result<()> {
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    let output = stdout_json(
        codex_command(home.path())?
            .args(["account", "auto", "on", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(output["enabled"], true);
    assert!(
        RegistryStore::new(home.path())
            .read()?
            .auto_selection
            .enabled
    );
    Ok(())
}

#[test]
fn unknown_disabled_and_logged_out_have_distinct_exit_codes() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ false)?;
    codex_command(fixture.home.path())?
        .args(["account", "show", "missing", "--json"])
        .assert()
        .code(10)
        .stderr(contains("unknownAccount"));
    codex_command(fixture.home.path())?
        .args(["account", "disable", "alpha"])
        .assert()
        .success();
    codex_command(fixture.home.path())?
        .args(["account", "use", "alpha", "--json"])
        .assert()
        .code(12)
        .stderr(contains("disabledAccount"));
    codex_command(fixture.home.path())?
        .args(["account", "use", "beta", "--json"])
        .assert()
        .code(13)
        .stderr(contains("notAuthenticated"));
    Ok(())
}

#[test]
fn ambiguous_reference_is_classified() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let mut registry = RegistryStore::new(fixture.home.path()).read()?;
    registry.accounts[1].alias = fixture.alpha.id.as_str().parse()?;
    let store = RegistryStore::new(fixture.home.path());
    store.compare_and_swap(
        /*expected_generation*/ 0,
        |current| *current = registry,
    )?;
    codex_command(fixture.home.path())?
        .args(["account", "show", fixture.alpha.id.as_str(), "--json"])
        .assert()
        .code(11)
        .stderr(contains("ambiguousAccount"));
    Ok(())
}

#[test]
fn stale_generation_is_rejected_without_mutation() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    codex_command(fixture.home.path())?
        .args([
            "account",
            "rename",
            "alpha",
            "renamed",
            "--expected-generation",
            "99",
            "--json",
        ])
        .assert()
        .code(15)
        .stderr(contains("generationConflict"));
    assert_eq!(
        RegistryStore::new(fixture.home.path())
            .read()?
            .lookup("alpha")?
            .alias
            .as_str(),
        "alpha"
    );
    Ok(())
}

#[test]
fn remove_requires_noninteractive_confirmation_and_preserves_on_refusal() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    codex_command(fixture.home.path())?
        .args(["account", "remove", "alpha", "--json"])
        .assert()
        .code(16)
        .stderr(contains("confirmationRequired"));
    assert!(
        RegistryStore::new(fixture.home.path())
            .read()?
            .lookup("alpha")
            .is_ok()
    );
    assert!(
        ProfileAuthStorage::new(
            fixture.home.path(),
            fixture.alpha.id,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?
        .load()?
        .is_some()
    );
    Ok(())
}

#[test]
fn remove_detects_in_use_then_deletes_credentials_and_selects_fallback() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let lock_path = fixture
        .home
        .path()
        .join("accounts")
        .join(fixture.alpha.id.as_str())
        .join(".profile-use-lock-v1");
    let use_lock = open_shared_lock(&lock_path)?;
    codex_command(fixture.home.path())?
        .args(["account", "remove", "alpha", "--yes", "--json"])
        .assert()
        .code(14)
        .stderr(contains("accountInUse"));
    drop(use_lock);

    let removed = stdout_json(
        codex_command(fixture.home.path())?
            .args(["account", "remove", "alpha", "--yes", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(removed["credentialsRemoved"], true);
    assert_eq!(removed["activeAccount"], "beta");
    let registry = RegistryStore::new(fixture.home.path()).read()?;
    assert!(registry.lookup("alpha").is_err());
    assert_eq!(registry.default_account_id, Some(fixture.beta.id));
    Ok(())
}

#[test]
fn doctor_reports_safe_health_without_paths() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let assertion = codex_command(fixture.home.path())?
        .args(["account", "doctor", "--json"])
        .assert()
        .success();
    let encoded = String::from_utf8(assertion.get_output().stdout.clone())?;
    assert!(!encoded.contains(fixture.home.path().to_string_lossy().as_ref()));
    let report: Value = serde_json::from_str(&encoded)?;
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["healthy"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_list_and_limits_preserve_multiple_buckets_and_reset_times() -> Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer access-alpha"))
        .and(header("chatgpt-account-id", "workspace-alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "plan_type": "pro",
            "rate_limit_reset_credits": {"available_count": 3},
            "credits": {"has_credits": true, "unlimited": false, "balance": "9.99"},
            "spend_control": {"reached": false, "individual_limit": {"limit":"25000", "used":"8000", "remaining":"17000", "used_percent":32, "remaining_percent":68, "reset_after_seconds":0, "reset_at":1893456000}},
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 42,
                    "limit_window_seconds": 300,
                    "reset_after_seconds": 0,
                    "reset_at": 1893456000
                },
                "secondary_window": {
                    "used_percent": 84,
                    "limit_window_seconds": 3600,
                    "reset_after_seconds": 0,
                    "reset_at": 1893542400
                }
            },
            "additional_rate_limits": [{
                "limit_name": "gpt-reserve",
                "metered_feature": "gpt-reserve",
                "rate_limit": {"allowed": true, "limit_reached": false,
                    "primary_window": {"used_percent": 35, "limit_window_seconds": 604800,
                        "reset_after_seconds": 0, "reset_at": 1893456000}}
            }, {
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex_bengalfox",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {
                        "used_percent": 0,
                        "limit_window_seconds": 18000,
                        "reset_after_seconds": 0,
                        "reset_at": 1893369600
                    }
                }
            }]
        })))
        .expect(3)
        .mount(&server)
        .await;
    let credits = [
        ("later", "available", Some("2030-02-01T00:00:00Z")),
        ("redeemed", "redeemed", Some("2029-01-01T00:00:00Z")),
        ("expired", "expired", Some("2025-01-01T00:00:00Z")),
        ("soonest", "available", Some("2030-01-01T03:00:00+03:00")),
        ("permanent", "available", None),
    ]
    .map(|(id, status, expires_at)| {
        serde_json::json!({
            "id": id, "status": status, "expires_at": expires_at,
            "reset_type": "codex_rate_limits", "granted_at": "2026-09-01T00:00:00Z"
        })
    });
    Mock::given(method("GET"))
        .and(path("/api/codex/rate-limit-reset-credits"))
        .and(header("authorization", "Bearer access-alpha"))
        .and(header("chatgpt-account-id", "workspace-alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "available_count": 3, "credits": credits,
        })))
        .expect(3)
        .mount(&server)
        .await;
    let fixture = fixture(/*beta_authenticated*/ true)?;
    std::fs::write(
        fixture.home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}\"\n",
            server.uri()
        ),
    )?;
    ProfileAuthStorage::new(
        fixture.home.path(),
        fixture.alpha.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&chatgpt_auth("alpha")?)?;
    let store = RegistryStore::new(fixture.home.path());
    store.compare_and_swap(/*expected_generation*/ 0, |registry| {
        registry.accounts[0].auth_mode = AuthMode::Chatgpt;
    })?;

    let report = stdout_json(
        codex_command(fixture.home.path())?
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(["account", "limits", "--all", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["accounts"][0]["alias"], "beta");
    assert_eq!(report["accounts"][0]["state"], "unknown");
    assert_eq!(report["accounts"][0]["reason"], "unsupportedAuthentication");
    assert_eq!(report["accounts"][1]["alias"], "alpha");
    assert_eq!(report["accounts"][1]["state"], "observed");
    assert_eq!(
        report["accounts"][1]["buckets"].as_array().map(Vec::len),
        Some(3)
    );

    let listed = stdout_json(
        codex_command(fixture.home.path())?
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(["account", "list", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(listed["accounts"][1]["limits"], report["accounts"][1]);
    assert_eq!(listed["accounts"][1]["limits"]["nextResetAt"], 1893456000);
    assert_eq!(
        listed["accounts"][1]["limits"]["nextResetScope"],
        "codex.primary"
    );
    assert_eq!(
        listed["accounts"][1]["limits"]["bankedResets"],
        serde_json::json!({
            "availableCount": 3, "soonestExpiresAt": 1893456000_i64, "expiryKnown": true,
        })
    );
    let before = chrono::Utc::now().timestamp();
    let human = codex_command(fixture.home.path())?
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .args(["account", "list"])
        .assert()
        .success();
    let human = String::from_utf8(human.get_output().stdout.clone())?;
    assert!(!human.contains("gpt-reserve"));
    assert!(!human.contains("Spark"));
    assert!(human.contains("2030-01-01 00:00:00 UTC"));
    let countdown = human
        .lines()
        .last()
        .unwrap()
        .split('\t')
        .next_back()
        .unwrap();
    assert_reset_countdown(countdown, /*reset*/ 1893456000, before)?;
    insta::assert_snapshot!(
        "account_list_limits",
        human.replace(countdown, "[Codex reset countdown]")
    );

    codex_command(fixture.home.path())?
        .args(["account", "limits", "beta", "--json"])
        .assert()
        .code(18)
        .stderr(contains("rateLimitsUnavailable"));
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let unavailable = stdout_json(
        codex_command(fixture.home.path())?
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(["account", "list", "--json"])
            .assert()
            .success(),
    )?;
    assert_eq!(
        unavailable["accounts"][1]["limits"]["reason"],
        "requestFailed"
    );
    assert_eq!(
        unavailable["accounts"][1]["limits"]["nextResetAt"],
        Value::Null
    );
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET")).and(path("/api/codex/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"plan_type":"pro", "rate_limit_reset_credits":{"available_count":0}, "spend_control":{"reached":true}, "rate_limit_reached_type":{"type":"workspace_owner_credits_depleted"}})))
        .expect(1).mount(&server).await;
    let restricted = codex_command(fixture.home.path())?
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .args(["account", "list"])
        .assert()
        .success();
    insta::assert_snapshot!(
        "account_list_spend_limit",
        String::from_utf8_lossy(&restricted.get_output().stdout)
    );
    server.verify().await;
    for (details, expected) in [
        (
            None,
            serde_json::json!({"availableCount": 2, "soonestExpiresAt": null, "expiryKnown": false}),
        ),
        (
            Some(serde_json::json!({"available_count": 0, "credits": []})),
            serde_json::json!({"availableCount": 0, "soonestExpiresAt": null, "expiryKnown": true}),
        ),
        (
            Some(serde_json::json!({"available_count": 1, "credits": [{
                "id": "permanent", "status": "available", "reset_type": "codex_rate_limits",
                "granted_at": "2026-09-01T00:00:00Z", "expires_at": null
            }]})),
            serde_json::json!({"availableCount": 1, "soonestExpiresAt": null, "expiryKnown": true}),
        ),
        (
            Some(serde_json::json!({"available_count": 1, "credits": [{
                "id": "invalid", "status": "available", "reset_type": "codex_rate_limits",
                "granted_at": "2026-09-01T00:00:00Z", "expires_at": "invalid-date"
            }]})),
            serde_json::json!({"availableCount": 1, "soonestExpiresAt": null, "expiryKnown": false}),
        ),
        (
            Some(serde_json::json!({"available_count": 2, "credits": []})),
            serde_json::json!({"availableCount": 2, "soonestExpiresAt": null, "expiryKnown": false}),
        ),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type": "pro", "rate_limit_reset_credits": {"available_count": 2},
                "rate_limit": {"allowed": true, "limit_reached": false,
                    "primary_window": {"used_percent": 42, "limit_window_seconds": 300,
                        "reset_after_seconds": 0, "reset_at": 1893456000}}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let response = match details {
            Some(details) => ResponseTemplate::new(200).set_body_json(details),
            None => ResponseTemplate::new(500),
        };
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(response)
            .expect(2)
            .mount(&server)
            .await;
        let output = stdout_json(
            codex_command(fixture.home.path())?
                .args(["account", "list", "--json"])
                .assert()
                .success(),
        )?;
        assert_eq!(output["accounts"][1]["limits"]["state"], "observed");
        assert_eq!(output["accounts"][1]["limits"]["bankedResets"], expected);
        let expiry_label = if expected["availableCount"] == 0 {
            "none"
        } else if expected["expiryKnown"] == true {
            "never"
        } else {
            "unknown"
        };
        let human = codex_command(fixture.home.path())?
            .args(["account", "list"])
            .assert()
            .success();
        let human = String::from_utf8(human.get_output().stdout.clone())?;
        let columns = human
            .lines()
            .last()
            .unwrap()
            .split('\t')
            .map(str::trim)
            .collect::<Vec<_>>();
        assert_eq!(
            (columns[6], columns[7]),
            (
                expected["availableCount"].to_string().as_str(),
                expiry_label
            )
        );
        assert!(human.contains("codex: 5m 42% used"));
        server.verify().await;
    }
    Ok(())
}

fn assert_reset_countdown(countdown: &str, reset: i64, before: i64) -> Result<()> {
    let mut seconds = 0;
    for part in countdown.split_whitespace() {
        let (number, unit) = part.split_at(part.len() - 1);
        let scale = match unit {
            "d" => 86400,
            "h" => 3600,
            "m" => 60,
            _ => panic!("invalid countdown: {countdown}"),
        };
        seconds += number.parse::<i64>()? * scale;
    }
    let after = chrono::Utc::now().timestamp();
    assert!(
        ((reset - after) / 60 * 60..=(reset - before) / 60 * 60).contains(&seconds),
        "{countdown} does not match main Codex reset {reset}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_list_keeps_each_accounts_weekly_reset_with_identical_unused_spark_limits()
-> Result<()> {
    let server = MockServer::start().await;
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let now = chrono::Utc::now().timestamp();
    std::fs::write(
        fixture.home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}\"\n",
            server.uri()
        ),
    )?;
    let accounts = [
        (&fixture.alpha, 100, now + 5 * 86400),
        (&fixture.beta, 61, now + 6 * 86400),
    ];
    for (account, used, reset) in accounts {
        ProfileAuthStorage::new(
            fixture.home.path(),
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?
        .save(&chatgpt_auth(account.alias.as_str())?)?;
        Mock::given(method("GET")).and(path("/api/codex/usage"))
            .and(header("authorization", format!("Bearer access-{}", account.alias)))
            .and(header("chatgpt-account-id", format!("workspace-{}", account.alias)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type": "pro",
                "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
                "rate_limit": {"allowed": used < 100, "limit_reached": used == 100,
                    "primary_window": {"used_percent": used, "limit_window_seconds": 604800, "reset_after_seconds": 0, "reset_at": reset}},
                "additional_rate_limits": [{"limit_name": "GPT-5.3-Codex-Spark", "metered_feature": "codex_bengalfox",
                    "rate_limit": {"allowed": true, "limit_reached": false,
                        "primary_window": {"used_percent": 0, "limit_window_seconds": 18000, "reset_after_seconds": 0, "reset_at": now + 18000}}}]
            }))).expect(2).mount(&server).await;
    }
    RegistryStore::new(fixture.home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            for account in &mut registry.accounts {
                account.auth_mode = AuthMode::Chatgpt;
            }
        },
    )?;
    let report = stdout_json(
        codex_command(fixture.home.path())?
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(["account", "list", "--json"])
            .assert()
            .success(),
    )?;
    let before = chrono::Utc::now().timestamp();
    let output = codex_command(fixture.home.path())?
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .args(["account", "list"])
        .assert()
        .success();
    let human = String::from_utf8(output.get_output().stdout.clone())?;
    let mut normalized = human.clone();
    for (account, _, reset) in accounts {
        let data = report["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["alias"] == account.alias.as_str())
            .unwrap();
        assert_eq!(data["limits"]["nextResetAt"], reset);
        assert_eq!(data["limits"]["nextResetScope"], "codex.primary");
        assert_eq!(data["limits"]["buckets"].as_array().unwrap().len(), 2);
        let line = human
            .lines()
            .find(|line| line.split('\t').nth(1).map(str::trim) == Some(account.alias.as_str()))
            .unwrap();
        let countdown = line.split('\t').next_back().unwrap();
        assert_reset_countdown(countdown, reset, before)?;
        normalized = normalized.replace(countdown, "[Codex reset countdown]");
    }
    insta::assert_snapshot!("account_list_weekly_resets", normalized);
    server.verify().await;
    Ok(())
}

#[test]
fn legacy_login_and_logout_command_shapes_remain_available() -> Result<()> {
    let home = TempDir::new()?;
    let help = codex_command(home.path())?.arg("--help").output()?;
    let text = String::from_utf8(help.stdout)?;
    assert!(text.contains("login"));
    assert!(text.contains("logout"));
    Ok(())
}

#[test]
fn login_and_logout_replace_only_the_active_profile() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let beta_profile = ProfileAuthStorage::new(
        fixture.home.path(),
        fixture.beta.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    let mut beta_auth = auth();
    beta_auth.openai_api_key = Some("beta-secret".to_string());
    beta_profile.save(&beta_auth)?;

    codex_command(fixture.home.path())?
        .args([
            "-c",
            "forced_login_method=\"api\"",
            "login",
            "--with-api-key",
        ])
        .write_stdin("active-replacement\n")
        .assert()
        .success();
    let alpha_profile = ProfileAuthStorage::new(
        fixture.home.path(),
        fixture.alpha.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?;
    assert_eq!(
        alpha_profile.load()?.and_then(|auth| auth.openai_api_key),
        Some("active-replacement".to_string())
    );
    assert_eq!(
        beta_profile.load()?.and_then(|auth| auth.openai_api_key),
        Some("beta-secret".to_string())
    );

    codex_command(fixture.home.path())?
        .arg("logout")
        .assert()
        .success();
    assert_eq!(alpha_profile.load()?, None);
    assert_eq!(
        beta_profile.load()?.and_then(|auth| auth.openai_api_key),
        Some("beta-secret".to_string())
    );
    Ok(())
}

fn open_shared_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    File::lock_shared(&file)?;
    Ok(file)
}

#[path = "account/reset_tests.rs"]
mod reset_tests;
