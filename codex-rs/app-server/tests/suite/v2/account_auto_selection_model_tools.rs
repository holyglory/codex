use super::*;
use app_test_support::write_models_cache_with_models;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolFunctionSpec;
use codex_app_server_protocol::DynamicToolNamespaceSpec;
use codex_app_server_protocol::DynamicToolNamespaceTool;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::TurnStartResponse;
use codex_protocol::models::FunctionCallOutputPayload;
use pretty_assertions::assert_eq;

enum CatalogSource {
    Cache,
    Remote,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_turn_loads_cached_profile_model_before_discovering_thread_tools() -> Result<()> {
    verify_profile_model_thread_tools(CatalogSource::Cache).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_turn_fetches_uncached_profile_model_before_discovering_thread_tools() -> Result<()>
{
    verify_profile_model_thread_tools(CatalogSource::Remote).await
}

async fn verify_profile_model_thread_tools(source: CatalogSource) -> Result<()> {
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    let model_name = "profile-only-thread-tool-model";
    MockResponsesConfig::new(&backend.uri())
        .with_model(model_name)
        .with_root_config(&format!(
            "chatgpt_base_url = \"{}\"\ncli_auth_credentials_store = \"file\"",
            backend.uri()
        ))
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = false")
        .write(codex_home.path())?;
    let profile =
        persist_managed_chatgpt_profile(codex_home.path(), "selected", /*priority*/ 1)?;
    let mut registry = AccountRegistry {
        default_account_id: Some(profile.metadata.id.clone()),
        accounts: vec![profile.metadata.clone()],
        ..AccountRegistry::default()
    };
    registry.auto_selection.enabled = true;
    RegistryStore::new(codex_home.path()).create(&registry)?;
    mount_observed_probe(&backend, &profile, /*used_percent*/ 10, 1..).await;

    let mut model = codex_models_manager::bundled_models_response()?
        .models
        .remove(0);
    model.slug = model_name.to_string();
    model.supports_search_tool = true;
    model.tool_mode = None;
    model.use_responses_lite = false;
    let mut global_model = model.clone();
    global_model.supports_search_tool = false;
    write_models_cache_with_models(codex_home.path(), vec![global_model])?;
    let profile_home = codex_home
        .path()
        .join("accounts")
        .join(profile.metadata.id.as_str());
    let expected_fetches = match source {
        CatalogSource::Cache => {
            write_models_cache_with_models(&profile_home, vec![model.clone()])?;
            0
        }
        CatalogSource::Remote => 1,
    };
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header(
            "authorization",
            format!("Bearer {}", profile.access_token),
        ))
        .and(header("chatgpt-account-id", profile.workspace_id.as_str()))
        .respond_with(ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
            "models": [model]
        })))
        .expect(expected_fetches)
        .mount(&backend)
        .await;

    let search_call_id = "find-thread-tool";
    let tool_call_id = "read-other-thread";
    let tool_args = json!({"threadId": "other-task"});
    let responses = responses::mount_response_sequence(
        &backend,
        vec![
            responses::sse(vec![
                responses::ev_response_created("search-response"),
                responses::ev_tool_search_call(
                    search_call_id,
                    &json!({"query": "read_thread", "limit": 1}),
                ),
                responses::ev_completed("search-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("tool-response"),
                responses::ev_function_call_with_namespace(
                    tool_call_id,
                    "codex_app",
                    "read_thread",
                    &serde_json::to_string(&tool_args)?,
                ),
                responses::ev_completed("tool-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("done-response"),
                responses::ev_assistant_message("done-message", "Read the other task."),
                responses::ev_completed("done-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("next-turn-response"),
                responses::ev_completed("next-turn-response"),
            ]),
        ]
        .into_iter()
        .map(|body| {
            responses::sse_response(body)
                .insert_header("x-codex-primary-used-percent", "10")
                .insert_header("x-codex-primary-window-minutes", "60")
                .insert_header(
                    "x-codex-primary-reset-at",
                    (chrono::Utc::now().timestamp() + 3600).to_string(),
                )
        })
        .collect(),
    )
    .await;
    let mut app_server = fresh_desktop_server(codex_home.path()).await?;
    let thread = app_server
        .start_thread(ThreadStartParams {
            model: Some(model_name.to_string()),
            dynamic_tools: Some(vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
                name: "codex_app".to_string(),
                description: "Read other Codex tasks.".to_string(),
                tools: vec![DynamicToolNamespaceTool::Function(
                    DynamicToolFunctionSpec {
                        name: "read_thread".to_string(),
                        description: "Read another task's messages and status.".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {"threadId": {"type": "string"}},
                            "required": ["threadId"],
                            "additionalProperties": false
                        }),
                        defer_loading: true,
                    },
                )],
            })]),
            ..Default::default()
        })
        .await?
        .thread;
    let turn_params = TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![V2UserInput::Text {
            text: "Read the other task".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    };
    let request_id = app_server
        .send_turn_start_request(turn_params.clone())
        .await?;
    let turn: TurnStartResponse = app_server.read_response(request_id).await?;
    let request = timeout(
        EVENT_TIMEOUT,
        app_server.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::DynamicToolCall { request_id, params } = request else {
        panic!("expected desktop thread tool call, got {request:?}");
    };
    assert_eq!(
        params,
        DynamicToolCallParams {
            thread_id: thread.id,
            turn_id: turn.turn.id,
            call_id: tool_call_id.to_string(),
            namespace: Some("codex_app".to_string()),
            tool: "read_thread".to_string(),
            arguments: tool_args,
        }
    );
    app_server
        .send_response(
            request_id,
            serde_json::to_value(DynamicToolCallResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "The other task is complete.".to_string(),
                }],
                success: true,
            })?,
        )
        .await?;
    let notification = timeout(
        EVENT_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let completed: TurnCompletedNotification =
        serde_json::from_value(notification.params.context("turn completion parameters")?)?;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let next_turn = app_server
        .start_turn_and_wait_for_completion(turn_params)
        .await?;
    assert_eq!(next_turn.turn.error, None);
    assert_eq!(next_turn.turn.status, TurnStatus::Completed);

    let requests = responses.requests();
    assert_eq!(requests.len(), 4);
    for request in [&requests[0], &requests[3]] {
        let body = request.body_json();
        assert_eq!(body["model"], model_name);
        assert!(
            body["tools"]
                .as_array()
                .context("model tools")?
                .iter()
                .any(|tool| tool["type"] == "tool_search")
        );
    }
    let search_output = requests[1]
        .input()
        .into_iter()
        .find(|item| item["type"] == "tool_search_output" && item["call_id"] == search_call_id)
        .context("thread tool discovery result")?;
    assert_eq!(search_output["tools"][0]["tools"][0]["name"], "read_thread");
    let output: FunctionCallOutputPayload =
        serde_json::from_value(requests[2].function_call_output(tool_call_id)["output"].clone())?;
    assert_eq!(
        output,
        FunctionCallOutputPayload::from_text("The other task is complete.".to_string())
    );
    backend.verify().await;
    Ok(())
}
