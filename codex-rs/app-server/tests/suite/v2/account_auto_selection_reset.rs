use super::*;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_turn_respects_the_ten_minute_reset_switch_threshold() -> Result<()> {
    for (current_reset, select_later) in [(6_601, false), (6_600, true)] {
        let codex_home = TempDir::new()?;
        let backend = MockServer::start().await;
        MockResponsesConfig::new(&backend.uri())
            .with_root_config(&format!(
                "chatgpt_base_url = \"{}\"\ncli_auth_credentials_store = \"file\"",
                backend.uri()
            ))
            .with_provider_config("requires_openai_auth = true\nsupports_websockets = false")
            .write(codex_home.path())?;
        write_models_cache(codex_home.path())?;
        let current =
            persist_managed_chatgpt_profile(codex_home.path(), "current", /*priority*/ 1_000)?;
        let mut peers = [
            persist_managed_chatgpt_profile(codex_home.path(), "peer-a", /*priority*/ 1_000)?,
            persist_managed_chatgpt_profile(codex_home.path(), "peer-b", /*priority*/ 1_000)?,
        ];
        peers.sort_by(|left, right| left.metadata.id.cmp(&right.metadata.id));
        let [earlier, later] = peers;
        let lower =
            persist_managed_chatgpt_profile(codex_home.path(), "lower", /*priority*/ 1)?;
        let mut registry = AccountRegistry {
            default_account_id: Some(current.metadata.id.clone()),
            accounts: [&current, &earlier, &later, &lower]
                .into_iter()
                .map(|profile| profile.metadata.clone())
                .collect(),
            ..AccountRegistry::default()
        };
        registry.auto_selection.enabled = true;
        RegistryStore::new(codex_home.path()).create(&registry)?;
        let now = chrono::Utc::now().timestamp();
        for (profile, reset_after) in [
            (&current, current_reset),
            (&earlier, 3_600),
            (&later, 7_200),
        ] {
            Mock::given(method("GET"))
                .and(path(RATE_LIMIT_PATH))
                .and(header(
                    "authorization",
                    format!("Bearer {}", profile.access_token),
                ))
                .and(header("chatgpt-account-id", profile.workspace_id.as_str()))
                .respond_with(ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
                    "plan_type": "pro",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "used_percent": 10,
                            "limit_window_seconds": 18000,
                            "reset_after_seconds": reset_after,
                            "reset_at": now + reset_after
                        }
                    }
                })))
                .expect(1)
                .mount(&backend)
                .await;
        }
        let selected = if select_later { &later } else { &current };
        Mock::given(method("POST"))
            .and(path(RESPONSES_PATH))
            .and(header(
                "authorization",
                format!("Bearer {}", selected.access_token),
            ))
            .and(header("chatgpt-account-id", selected.workspace_id.as_str()))
            .respond_with(
                responses::sse_response(responses::sse(vec![
                    responses::ev_response_created("latest-reset-response"),
                    responses::ev_assistant_message(
                        "latest-reset-message",
                        "Selected with the ten-minute margin.",
                    ),
                    responses::ev_completed("latest-reset-response"),
                ]))
                .insert_header("x-codex-primary-used-percent", "10")
                .insert_header("x-codex-primary-window-minutes", "300")
                .insert_header(
                    "x-codex-primary-reset-at",
                    (now + if select_later { 7_200 } else { current_reset }).to_string(),
                ),
            )
            .expect(2)
            .mount(&backend)
            .await;

        let mut app_server = fresh_desktop_server(codex_home.path()).await?;
        let first = start_first_turn(&mut app_server).await?;
        assert_eq!(first.turn.status, TurnStatus::Completed);
        let second = app_server
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: first.thread_id,
                input: vec![V2UserInput::Text {
                    text: "continue with fresh cached resets".to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        assert_eq!(second.turn.status, TurnStatus::Completed);
        let requests = backend
            .received_requests()
            .await
            .context("backend requests")?;
        let first_model = requests
            .iter()
            .position(|request| request.method == "POST" && request.url.path() == RESPONSES_PATH)
            .context("model request")?;
        let probe_count = requests[..first_model]
            .iter()
            .filter(|request| request.url.path() == RATE_LIMIT_PATH)
            .count();
        assert_eq!(probe_count, 3);
        for profile in [&current, &earlier, &later] {
            assert_eq!(profile_probe_count(&requests, profile), 1);
        }
        assert_eq!(profile_probe_count(&requests, &lower), 0);
        backend.verify().await;
    }
    Ok(())
}
