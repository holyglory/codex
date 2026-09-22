use super::*;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

async fn reset_fixture(server: &MockServer) -> Result<Fixture> {
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
        fixture.beta.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&chatgpt_auth("beta")?)?;
    RegistryStore::new(fixture.home.path())
        .compare_and_swap(/*expected_generation*/ 0, |registry| {
            registry.accounts[1].auth_mode = AuthMode::Chatgpt
        })?;
    Ok(fixture)
}

fn credit(id: &str, expiry: Option<&str>) -> Value {
    serde_json::json!({"id": id, "status": "available", "reset_type": "codex_rate_limits",
        "granted_at": "2026-01-01T00:00:00Z", "expires_at": expiry})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_reset_recovers_lost_reply_without_consuming_another_credit() -> Result<()> {
    for delayed in [false, true] {
        let server = MockServer::start().await;
        let fixture = reset_fixture(&server).await?;
        let before = RegistryStore::new(fixture.home.path()).read()?;
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let consumption = Arc::new(Mutex::new(0));
        let pending = Arc::clone(&consumption);
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .and(header("authorization", "Bearer access-beta"))
            .and(header("chatgpt-account-id", "workspace-beta"))
            .respond_with(move |_: &wiremock::Request| {
                let used = *pending.lock().unwrap();
                let mut credits = vec![credit("never", None)];
                if used == 0 {
                    credits.push(credit("soon", Some("2030-01-01T00:00:00Z")));
                }
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"available_count": credits.len(), "credits": credits}),
                )
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type": "pro", "rate_limit": {"allowed": true, "limit_reached": false,
                    "primary_window": {"used_percent": 0, "limit_window_seconds": 300,
                    "reset_after_seconds": 0, "reset_at": 1893456000}}
            })))
            .mount(&server)
            .await;
        let captured = Arc::clone(&requests);
        let consumed = Arc::clone(&consumption);
        Mock::given(method("POST"))
            .and(path("/api/codex/rate-limit-reset-credits/consume"))
            .and(header("authorization", "Bearer access-beta"))
            .and(header("chatgpt-account-id", "workspace-beta"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let mut calls = captured.lock().unwrap();
                if calls.is_empty() {
                    *consumed.lock().unwrap() += 1;
                } else {
                    assert_eq!(body, calls[0]);
                }
                calls.push(body);
                if delayed && calls.len() == 1 {
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_secs(11))
                        .set_body_json(serde_json::json!({"code":"reset", "windows_reset":2}))
                } else if !delayed && calls.len() <= 2 {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"code":"already_redeemed", "windows_reset":2}),
                    )
                }
            })
            .expect(3)
            .mount(&server)
            .await;
        let output = stdout_json(
            codex_command(fixture.home.path())?
                .args(["account", "reset", fixture.beta.id.as_str(), "--json"])
                .assert()
                .code(if delayed { 0 } else { 27 }),
        )?;
        assert_eq!(output["account"], "beta");
        assert_eq!(output["creditId"], "soon");
        assert_eq!(
            output["outcome"],
            if delayed {
                "alreadyRedeemed"
            } else {
                "uncertain"
            }
        );
        let replay = stdout_json(
            codex_command(fixture.home.path())?
                .args([
                    "account",
                    "reset",
                    "beta",
                    "--credit-id",
                    "soon",
                    "--request-id",
                    output["requestId"].as_str().unwrap(),
                    "--json",
                ])
                .assert()
                .success(),
        )?;
        assert_eq!(replay["outcome"], "alreadyRedeemed");
        assert_eq!(
            replay["refreshedBankedResets"],
            serde_json::json!({"availableCount":1, "soonestExpiresAt":null, "expiryKnown":true})
        );
        assert_eq!(replay["refreshedUsage"][0]["primary"]["used_percent"], 0.0);
        assert_eq!(*consumption.lock().unwrap(), 1);
        assert_eq!(
            serde_json::to_value(RegistryStore::new(fixture.home.path()).read()?)?,
            serde_json::to_value(before)?
        );
        server.verify().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_reset_filters_invalid_credits_and_retains_backend_outcomes() -> Result<()> {
    for (code, exit) in [
        ("reset", 0),
        ("nothing_to_reset", 0),
        ("already_redeemed", 0),
        ("no_credit", 25),
    ] {
        let server = MockServer::start().await;
        let fixture = reset_fixture(&server).await?;
        let mut unsupported = credit("unsupported", Some("2029-01-01T00:00:00Z"));
        unsupported["reset_type"] = "future_kind".into();
        let credits = vec![
            credit("expired", Some("2025-01-01T00:00:00Z")),
            credit("invalid", Some("bad-date")),
            unsupported,
            credit("permanent", None),
            credit("z-tie", Some("2030-01-01T00:00:00Z")),
            credit("a-tie", Some("2030-01-01T03:00:00+03:00")),
        ];
        Mock::given(method("GET"))
            .and(path("/api/codex/rate-limit-reset-credits"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"available_count":6,"credits":credits})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .respond_with(ResponseTemplate::new(500))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/codex/rate-limit-reset-credits/consume"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"credit_id":"a-tie"}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"code":code,"windows_reset":0})),
            )
            .expect(2)
            .mount(&server)
            .await;
        let output = stdout_json(
            codex_command(fixture.home.path())?
                .args(["account", "reset", "beta", "--json"])
                .assert()
                .code(exit),
        )?;
        assert_eq!(output["refreshError"], "usageUnavailable");
        assert_eq!(
            output["outcome"],
            match code {
                "nothing_to_reset" => "nothingToReset",
                "already_redeemed" => "alreadyRedeemed",
                "no_credit" => "noCredit",
                _ => "reset",
            }
        );
        let human = codex_command(fixture.home.path())?
            .args([
                "account",
                "reset",
                "beta",
                "--credit-id",
                "a-tie",
                "--request-id",
                output["requestId"].as_str().unwrap(),
            ])
            .assert()
            .code(exit);
        let human = String::from_utf8(human.get_output().stdout.clone())?;
        insta::assert_snapshot!(
            format!("account_reset_{code}"),
            human.replace(output["requestId"].as_str().unwrap(), "[request ID]")
        );
        for id in ["expired", "invalid", "unsupported", "missing"] {
            codex_command(fixture.home.path())?
                .args(["account", "reset", "beta", "--credit-id", id])
                .assert()
                .code(25);
        }
        server.verify().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_reset_rejects_unavailable_accounts_without_consumption() -> Result<()> {
    let server = MockServer::start().await;
    let fixture = reset_fixture(&server).await?;
    codex_command(fixture.home.path())?
        .args(["account", "reset", "missing"])
        .assert()
        .code(10);
    codex_command(fixture.home.path())?
        .args(["account", "reset", "alpha"])
        .assert()
        .code(28);
    codex_command(fixture.home.path())?
        .args(["account", "disable", "beta"])
        .assert()
        .success();
    codex_command(fixture.home.path())?
        .args(["account", "reset", "beta"])
        .assert()
        .code(12);
    assert!(server.received_requests().await.unwrap().is_empty());
    Ok(())
}
