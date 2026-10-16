use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_credit_fallback_obeys_priority_permission_and_probe_evidence() -> Result<()> {
    for (case, peer_usage, enabled, denied, expected_alias) in [
        ("included peer", Some(10), true, false, "beta"),
        ("same tier exhausted", Some(100), true, false, "alpha"),
        ("permission off", Some(100), false, false, "gamma"),
        ("peer unknown", None, true, false, "gamma"),
        ("spend control", Some(100), true, true, "gamma"),
    ] {
        let home = TempDir::new()?;
        let backend = MockServer::start().await;
        write_test_config(home.path(), &backend.uri()).await?;
        let [alpha, beta, gamma] = persist_cli_profile_set(home.path())?;
        RegistryStore::new(home.path()).compare_and_swap(
            /*expected_generation*/ 0,
            |registry| {
                registry.accounts[0].priority = 2;
                registry.accounts[0].credit_usage_enabled = enabled;
            },
        )?;
        Mock::given(method("GET")).and(path(RATE_LIMIT_PATH))
            .and(header("authorization", format!("Bearer {}", alpha.access_token)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type":"pro", "credits":{"has_credits":true,"unlimited":false,"balance":"9.99"},
                "spend_control":{"reached":denied},
                "rate_limit":{"allowed":false,"limit_reached":true,"primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_after_seconds":300,"reset_at":chrono::Utc::now().timestamp()+300}}
            }))).expect(1).mount(&backend).await;
        if let Some(used) = peer_usage {
            mount_observed_probe(&backend, &beta, used, /*expected*/ 1).await;
        } else {
            mount_failed_probe(&backend, &beta, /*expected*/ 1..=2).await;
        }
        mount_observed_probe(
            &backend,
            &gamma,
            /*used_percent*/ 10,
            /*expected*/ if expected_alias == "gamma" { 1 } else { 0 },
        )
        .await;
        Mock::given(method("GET"))
            .and(path(RESPONSES_PATH))
            .respond_with(ResponseTemplate::new(426))
            .expect(1)
            .mount(&backend)
            .await;
        let expected = match expected_alias {
            "alpha" => &alpha,
            "beta" => &beta,
            _ => &gamma,
        };
        Mock::given(method("POST"))
            .and(path(RESPONSES_PATH))
            .and(header(
                "authorization",
                format!("Bearer {}", expected.access_token),
            ))
            .and(header("chatgpt-account-id", expected.workspace_id.as_str()))
            .respond_with(responses::sse_response(
                create_final_assistant_message_sse_response("credit routing verified")?,
            ))
            .expect(1)
            .mount(&backend)
            .await;
        let mut server = fresh_desktop_server(home.path()).await?;
        let completed = start_first_turn(&mut server).await?;
        assert_eq!(completed.turn.status, TurnStatus::Completed, "{case}");
        backend.verify().await;
    }
    Ok(())
}
