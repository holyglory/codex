use super::*;
use pretty_assertions::assert_eq;

#[test]
fn credit_permission_is_per_account_persistent_and_generation_checked() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let store = RegistryStore::new(fixture.home.path());
    let before = store.read()?;
    assert!(
        before
            .accounts
            .iter()
            .all(|account| !account.credit_usage_enabled)
    );
    for (mode, generation, changed) in [
        ("enabled", 1, true),
        ("enabled", 1, false),
        ("disabled", 2, true),
    ] {
        let output = stdout_json(
            codex_command(fixture.home.path())?
                .args(["account", "edit", "alpha", "--credit-usage", mode, "--json"])
                .assert()
                .success(),
        )?;
        assert_eq!(
            (output["generation"].as_u64(), output["changed"].as_bool()),
            (Some(generation), Some(changed))
        );
        let expected = mode == "enabled";
        let shown = stdout_json(
            codex_command(fixture.home.path())?
                .args(["account", "show", "alpha", "--json"])
                .assert()
                .success(),
        )?;
        assert_eq!(shown["account"]["creditUsageEnabled"], expected);
        let saved = store.read()?;
        assert_eq!(
            saved
                .accounts
                .iter()
                .map(|account| account.credit_usage_enabled)
                .collect::<Vec<_>>(),
            [expected, false]
        );
    }
    codex_command(fixture.home.path())?
        .args([
            "account",
            "edit",
            "beta",
            "--credit-usage",
            "enabled",
            "--expected-generation",
            "0",
            "--json",
        ])
        .assert()
        .failure()
        .stderr(contains("generationConflict"));
    assert!(
        store
            .read()?
            .accounts
            .iter()
            .all(|account| !account.credit_usage_enabled)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_list_displays_credit_balances_and_permission() -> Result<()> {
    let fixture = fixture(/*beta_authenticated*/ true)?;
    let backend = MockServer::start().await;
    std::fs::write(
        fixture.home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"{}\"\n",
            backend.uri()
        ),
    )?;
    ProfileAuthStorage::new(
        fixture.home.path(),
        fixture.alpha.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&chatgpt_auth("alpha")?)?;
    RegistryStore::new(fixture.home.path()).compare_and_swap(
        /*expected_generation*/ 0,
        |registry| {
            registry.accounts[0].auth_mode = AuthMode::Chatgpt;
            registry.accounts[0].credit_usage_enabled = true;
        },
    )?;
    for (credits, expected) in [
        (
            serde_json::json!({"has_credits":true,"unlimited":false,"balance":"12.345"}),
            "12.345",
        ),
        (
            serde_json::json!({"has_credits":true,"unlimited":false,"balance":null}),
            "available",
        ),
        (
            serde_json::json!({"has_credits":false,"unlimited":true,"balance":null}),
            "unlimited",
        ),
        (
            serde_json::json!({"has_credits":false,"unlimited":false,"balance":"0"}),
            "0",
        ),
        (
            serde_json::json!({"has_credits":true,"unlimited":false,"balance":"bad\nvalue"}),
            "available",
        ),
        (Value::Null, "unknown"),
    ] {
        backend.reset().await;
        Mock::given(method("GET")).and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type":"pro", "credits":credits,
                "rate_limit_reset_credits":{"available_count":0},
                "rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":300,"reset_at":1893456000}}
            }))).expect(2).mount(&backend).await;
        let out = codex_command(fixture.home.path())?
            .args(["account", "list"])
            .assert()
            .success();
        let human = String::from_utf8(out.get_output().stdout.clone())?;
        let rows = account_table(&human);
        assert_eq!(
            (rows[0]["CREDITS"].as_str(), rows[0]["CREDIT USE"].as_str()),
            (expected, "enabled")
        );
        assert_eq!(
            (rows[1]["CREDITS"].as_str(), rows[1]["CREDIT USE"].as_str()),
            ("unknown", "disabled")
        );
        let listed = stdout_json(
            codex_command(fixture.home.path())?
                .args(["account", "list", "--json"])
                .assert()
                .success(),
        )?;
        let account = listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["alias"] == "alpha")
            .unwrap();
        assert_eq!(account["creditUsageEnabled"], true);
        assert_eq!(account["limits"]["buckets"][0]["credits"], credits);
        backend.verify().await;
    }
    Ok(())
}
