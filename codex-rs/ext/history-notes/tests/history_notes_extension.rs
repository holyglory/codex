use std::sync::Arc;

use codex_config::LoaderOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::TokenBudgetConfig;
use codex_extension_api::ContentItemKind;
use codex_extension_api::ConversationHistory;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::NoopTurnItemEmitter;
use codex_extension_api::PromptFragment;
use codex_extension_api::PromptSlot;
use codex_extension_api::StepPreparationInput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallSource;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolPayload;
use codex_features::RuntimeAuthMode;
use codex_history_notes_extension::install;
use codex_http_client::DestinationPolicy;
use codex_http_client::NetworkPolicyController;
use codex_login::AuthHeaders;
use codex_login::AuthManager;
use codex_login::AuthManagerLease;
use codex_login::CodexAuth;
use codex_model_provider::ProviderAuthContext;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::NotesToolMessages;
use codex_protocol::openai_models::ToolMessage;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TruncationPolicy;
use codex_runtime_agent_instance::AgentHostAuthorization;
use codex_runtime_agent_instance::RuntimeAuthManager;
use codex_runtime_agent_instance::RuntimeRegistrationMetadata;
use codex_runtime_agent_instance::RuntimeRole;
use core_test_support::runtime_auth::PROBE_SERVER;
use core_test_support::runtime_auth::run_harness_through_proxy;
use http::HeaderMap;
use http::HeaderValue;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const THREAD_HINT: &str = "Recent notes (up to 5, most-recent first):\n- /root/worker/notes/latest.md (2 lines, 14 UTF-8 bytes)";

#[tokio::test]
async fn installed_extension_exposes_and_invokes_history_notes_tools() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/read_file"))
        .and(header("x-openai-actor-authorization", "actor-biscuit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted_output": "enc_payload"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
        .and(header("x-openai-actor-authorization", "actor-biscuit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": THREAD_HINT})))
        .mount(&server)
        .await;
    let codex_home = TempDir::new()?;
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    config.model_provider.base_url = Some(format!("{}/backend-api/codex", server.uri()));
    config.token_budget = Some(TokenBudgetConfig {
        use_history_notes_extension: true,
        ..TokenBudgetConfig::default()
    });
    let controller = NetworkPolicyController::default();
    config.application_network_policy = controller.policy();
    assert!(controller.publish(
        controller.policy().revision(),
        DestinationPolicy::Unrestricted
    ));

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-openai-actor-authorization",
        HeaderValue::from_static("actor-biscuit"),
    );
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::Headers(AuthHeaders::new(headers)));
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    install(&mut builder, auth_manager);
    let registry = builder.build();
    let session_store = ExtensionData::new("session-123");
    let thread_store = ExtensionData::new("thread-123");
    let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: Some(AgentPath::root().join("worker").expect("agent path")),
        agent_nickname: None,
        agent_role: None,
    });

    for contributor in registry.thread_lifecycle_contributors() {
        contributor
            .on_thread_start(ThreadStartInput {
                config: &config,
                session_source: &session_source,
                persistent_thread_state_available: true,
                environments: &[],
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &session_store,
                thread_store: &thread_store,
            })
            .await;
    }

    let tools = exposed_tools(&registry, &session_store, &thread_store);
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.tool_name())
            .collect::<Vec<_>>(),
        vec![
            ToolName::namespaced("history", "list_windows"),
            ToolName::namespaced("history", "list_items"),
            ToolName::namespaced("history", "read_item"),
            ToolName::namespaced("history", "search_contents"),
            ToolName::namespaced("notes", "list_files_by_prefix"),
            ToolName::namespaced("notes", "read_file"),
            ToolName::namespaced("notes", "search_contents"),
            ToolName::namespaced("notes", "append_to_file"),
            ToolName::namespaced("notes", "write_file"),
        ]
    );

    // Existing executors retain the catalog snapshot selected for their step.
    let step_store = ExtensionData::new("step-1");
    let mut messages = NotesToolMessages {
        read_file: Some(ToolMessage {
            description: Some("Catalog read instructions".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    step_store.insert(messages.clone());
    let catalog_tools =
        registry.tool_contributors()[0].tools_for_step(&session_store, &thread_store, &step_store);
    let captured_tool = catalog_tools
        .iter()
        .find(|tool| tool.tool_name() == ToolName::namespaced("notes", "read_file"))
        .expect("notes.read_file should be exposed");
    let captured_spec = captured_tool.spec();
    messages.read_file.as_mut().expect("read_file").description = Some(String::new());
    step_store.insert(messages);
    let next_tools =
        registry.tool_contributors()[0].tools_for_step(&session_store, &thread_store, &step_store);
    assert_ne!(
        next_tools
            .iter()
            .find(|tool| tool.tool_name() == ToolName::namespaced("notes", "read_file"))
            .expect("notes.read_file should be exposed")
            .spec(),
        captured_spec
    );
    assert_eq!(captured_tool.spec(), captured_spec);

    let read_file = tools
        .iter()
        .find(|tool| tool.tool_name() == ToolName::namespaced("notes", "read_file"))
        .expect("notes.read_file should be exposed");
    let call = tool_call(
        ToolName::namespaced("notes", "read_file"),
        json!({"path": "notes.md"}),
    );
    let output = read_file.handle(call.clone()).await?;
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item(&call.call_id, &call.payload)
    else {
        panic!("expected function-call output");
    };
    assert_eq!(
        output.content_items(),
        Some(
            [FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "enc_payload".to_string(),
            }]
            .as_slice()
        )
    );

    let hints = registry.context_contributors()[0]
        .contribute_thread_context(&session_store, &thread_store)
        .await;
    assert_eq!(
        hints,
        vec![PromptFragment::tool(
            PromptSlot::ContextWindow,
            THREAD_HINT,
            ContentItemKind("notes.thread_hint".to_string()),
            "notes".to_string(),
        )]
    );

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body)?,
        json!({
            "path": "notes.md",
            "context": {
                "session_id": "session-123",
                "current_agent_name": "/root/worker",
            }
        })
    );

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[1].body)?,
        json!({
            "context": {
                "session_id": "session-123",
                "current_agent_name": "/root/worker",
            }
        })
    );

    for (namespace, name, mut arguments) in [
        ("history", "list_windows", json!({"limit": 101})),
        ("history", "list_items", json!({})),
        (
            "history",
            "list_items",
            json!({"limit": 21, "max_chars_per_item": 4_000}),
        ),
        (
            "history",
            "read_item",
            json!({"window_id": "window", "item_id": "item", "limit_chars": 20_001}),
        ),
        (
            "history",
            "search_contents",
            json!({"query": "x".repeat(1_001), "limit": 21}),
        ),
        ("history", "search_contents", json!({"query": ""})),
        ("notes", "list_files_by_prefix", json!({"max_results": 101})),
        (
            "notes",
            "search_contents",
            json!({"query": "x".repeat(1_001), "max_files": 21, "max_matches_per_file": 11}),
        ),
        ("notes", "search_contents", json!({"query": ""})),
        (
            "notes",
            "read_file",
            json!({"path": "notes.md", "start_line": -2}),
        ),
        (
            "notes",
            "append_to_file",
            json!({"path": "notes.md", "text": "append"}),
        ),
        (
            "notes",
            "write_file",
            json!({"path": "notes.md", "text": "replace"}),
        ),
    ] {
        server.reset().await;
        let mut response = json!({"encrypted_output": "enc_history"});
        let mut expected_output =
            vec![json!({"type": "encrypted_content", "encrypted_content": "enc_history"})];
        if namespace == "history" && name == "read_item" {
            // Images are not part of the text budget sent to the backend.
            let data = "cG5n".repeat(1_000);
            response["images"] = json!([
                {"data": data, "mime_type": "image/png", "detail": "original"},
                {"data": "anBlZw==", "mime_type": "image/jpeg", "detail": "high"}
            ]);
            expected_output.extend([
                json!({"type": "input_image", "image_url": format!("data:image/png;base64,{data}"), "detail": "original"}),
                json!({"type": "input_image", "image_url": "data:image/jpeg;base64,anBlZw==", "detail": "high"})
            ]);
        }
        Mock::given(method("POST"))
            .and(path(format!(
                "/backend-api/codex/alpha/{namespace}/v2/{name}"
            )))
            .and(header("x-openai-actor-authorization", "actor-biscuit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        let tool_name = ToolName::namespaced(namespace, name);
        let tool = tools
            .iter()
            .find(|tool| tool.tool_name() == tool_name)
            .expect("exposed tool");
        let call = tool_call(tool_name, arguments.clone());
        let output = tool.handle(call.clone()).await?;
        assert_eq!(
            serde_json::to_value(output.to_response_item(&call.call_id, &call.payload))?,
            json!({"type": "function_call_output", "call_id": call.call_id, "output": expected_output})
        );
        arguments["context"] = json!({
            "session_id": "session-123",
            "current_agent_name": "/root/worker",
        });
        let requests = server.received_requests().await.expect("recorded requests");
        for request in &requests {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(
                    request
                        .headers
                        .get("x-openai-tool-output-truncation-policy")
                        .expect("truncation policy header")
                        .to_str()?
                )?,
                json!({"mode": "bytes", "limit": 1024})
            );
        }
        assert_eq!(
            requests
                .iter()
                .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body))
                .collect::<Result<Vec<_>, _>>()?,
            vec![arguments]
        );
    }

    // Plaintext backend responses must keep attachments out of text and logs too.
    server.reset().await;
    let text_result = json!({"content": "look: [image 1]", "n_chars": 15, "next_offset_chars": 15});
    let mut response = text_result.clone();
    response["images"] = json!([
        {"data": "cG5n", "mime_type": "image/png", "detail": "original"}
    ]);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/history/v2/read_item"))
        .and(header("x-openai-actor-authorization", "actor-biscuit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(&server)
        .await;
    let tool_name = ToolName::namespaced("history", "read_item");
    let read_item = tools
        .iter()
        .find(|tool| tool.tool_name() == tool_name)
        .expect("history.read_item");
    let call = tool_call(tool_name, json!({"window_id": "window", "item_id": "item"}));
    let output = read_item.handle(call.clone()).await?;
    assert_eq!(output.log_output(), text_result.to_string());
    assert_eq!(
        serde_json::to_value(output.to_response_item(&call.call_id, &call.payload))?,
        json!({
            "type": "function_call_output",
            "call_id": call.call_id,
            "output": [
                {"type": "input_text", "text": text_result.to_string()},
                {"type": "input_image", "image_url": "data:image/png;base64,cG5n", "detail": "original"}
            ]
        })
    );

    for result in [
        json!({"text": ""}),
        json!({"text": "x".repeat(4_001)}),
        json!({"encrypted_output": "old-backend-hint"}),
    ] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
            .respond_with(ResponseTemplate::new(200).set_body_json(result))
            .mount(&server)
            .await;
        assert!(
            registry.context_contributors()[0]
                .contribute_thread_context(&session_store, &thread_store)
                .await
                .is_empty()
        );
    }

    server.reset().await;
    assert!(controller.publish(
        controller.policy().revision(),
        DestinationPolicy::Restricted {
            allowed_hosts: Default::default(),
        },
    ));
    assert!(
        registry.context_contributors()[0]
            .contribute_thread_context(&session_store, &thread_store)
            .await
            .is_empty()
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );

    let mut disabled_config = config.clone();
    disabled_config.token_budget = None;
    for contributor in registry.config_contributors() {
        contributor.on_config_changed(&session_store, &thread_store, &config, &disabled_config);
    }
    assert!(exposed_tools(&registry, &session_store, &thread_store).is_empty());
    assert!(
        registry.context_contributors()[0]
            .contribute_thread_context(&session_store, &thread_store)
            .await
            .is_empty()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_notes_keep_thread_runtime_auth_and_step_account_leases() -> TestResult {
    core_test_support::skip_if_no_network!(Ok(()));
    if std::env::var(PROBE_SERVER).is_ok() {
        let codex_home = TempDir::new()?;
        let mut config = ConfigBuilder::default()
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        config.model_provider.base_url = Some("https://chatgpt.com/backend-api/codex".into());
        config.token_budget = Some(TokenBudgetConfig {
            use_history_notes_extension: true,
            ..TokenBudgetConfig::default()
        });
        let controller = NetworkPolicyController::default();
        config.application_network_policy = controller.policy();
        assert!(controller.publish(
            controller.policy().revision(),
            DestinationPolicy::Unrestricted,
        ));
        let auth_for = |account| {
            AuthManager::from_auth_for_testing(
                CodexAuth::from_external_chatgpt_tokens(
                    "e30.e30.c2ln",
                    account,
                    Some("enterprise"),
                )
                .expect("fake ChatGPT auth should parse"),
            )
        };
        let credentials = auth_for("thread-account");
        let runtime = Arc::new(RuntimeAuthManager::new(
            Arc::clone(&credentials),
            AgentHostAuthorization::InstallationId {
                installation_id: "00000000-0000-0000-0000-000000000000".parse()?,
            },
            RuntimeRegistrationMetadata {
                harness: Some("codex".into()),
                harness_version: None,
                role: RuntimeRole::Orchestrator,
            },
            "https://auth.openai.com/agent/register".parse()?,
        ));
        for mode in [
            None,
            Some(RuntimeAuthMode::DualHeader),
            Some(RuntimeAuthMode::AssertionOnly),
        ] {
            let mut builder = ExtensionRegistryBuilder::<Config>::new();
            install(
                &mut builder,
                AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unrelated-key")),
            );
            let registry = builder.build();
            let session_store = ExtensionData::new("session");
            let thread_store = ExtensionData::new("thread");
            let turn_store = ExtensionData::new("turn");
            thread_store.insert(ProviderAuthContext {
                credentials: Arc::clone(&credentials),
                runtime_auth: mode.map(|mode| (Arc::clone(&runtime), mode)),
            });
            for contributor in registry.thread_lifecycle_contributors() {
                contributor
                    .on_thread_start(ThreadStartInput {
                        config: &config,
                        session_source: &SessionSource::Cli,
                        persistent_thread_state_available: true,
                        environments: &[],
                        mcp_resource_client: None,
                        extension_metrics: None,
                        session_store: &session_store,
                        thread_store: &thread_store,
                    })
                    .await;
            }
            assert_eq!(
                registry.context_contributors()[0]
                    .contribute_thread_context(&session_store, &thread_store)
                    .await
                    .len(),
                1
            );
            let contributor = &registry.tool_contributors()[0];
            let mut captured = vec![
                contributor.tools(&session_store, &thread_store),
                contributor.tools_for_step(
                    &session_store,
                    &thread_store,
                    &ExtensionData::new("unleased-step"),
                ),
            ];
            // Runtime-owned auth excludes account-profile switching. Its legacy lease
            // must still retain the thread runtime; ordinary auth can select other owners.
            for account in ["first-account", "second-account"] {
                let selected = match mode {
                    Some(_) => Arc::clone(&credentials),
                    None => auth_for(account),
                };
                turn_store.insert(AuthManagerLease::legacy(selected));
                let step_store = ExtensionData::new(account);
                contributor
                    .prepare_for_step(StepPreparationInput {
                        turn_id: "turn",
                        ready_selected_capability_roots: &[],
                        executor_capability_discovery: None,
                        executor_capability_discovery_v2: None,
                        selected_plugins: &Default::default(),
                        session_store: &session_store,
                        thread_store: &thread_store,
                        turn_store: &turn_store,
                        step_store: &step_store,
                    })
                    .await;
                captured.push(contributor.tools_for_step(
                    &session_store,
                    &thread_store,
                    &step_store,
                ));
            }
            // Invoke only after the next account is selected to check snapshot ownership.
            for tools in captured {
                let tool = tools
                    .iter()
                    .find(|tool| tool.tool_name() == ToolName::namespaced("notes", "read_file"))
                    .expect("thread credentials should enable notes.read_file");
                let call = tool_call(
                    ToolName::namespaced("notes", "read_file"),
                    json!({"path": "notes.md"}),
                );
                let output = tool.handle(call.clone()).await?;
                assert_eq!(
                    serde_json::to_value(output.to_response_item(&call.call_id, &call.payload))?,
                    json!({"type": "function_call_output", "call_id": call.call_id,
                        "output": [{"type": "encrypted_content", "encrypted_content": "enc_payload"}]}),
                );
            }
        }
        return Ok(());
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agent/register"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
            "runtime_agent_instance_id": "runtime-history", "agent_host_id": "host",
            "actor": {"kind": "chatgpt", "user_id": "user", "account_id": "thread-account"},
        })))
        .expect(/*r*/ 1)
        .mount(&server)
        .await;
    for (account, count) in [
        ("thread-account", 10),
        ("first-account", 1),
        ("second-account", 1),
    ] {
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/alpha/notes/v2/read_file"))
            .and(header("chatgpt-account-id", account))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
                "encrypted_output": "enc_payload",
            })))
            .expect(count)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
        .and(header("chatgpt-account-id", "thread-account"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"text": THREAD_HINT})))
        .expect(/*r*/ 3)
        .mount(&server)
        .await;
    run_harness_through_proxy(
        &server,
        "history_notes_keep_thread_runtime_auth_and_step_account_leases",
        /*realtime_target*/ None,
    )
    .await?;
    let requests = server.received_requests().await.expect("recorded requests");
    let mut counts = (0, 0, 0);
    for request in requests
        .iter()
        .filter(|request| request.url.path().contains("/alpha/"))
    {
        let authorization = request.headers["authorization"].to_str()?;
        if request.headers.contains_key("x-openai-agent-assertion") {
            assert_eq!(authorization, "Bearer e30.e30.c2ln");
            counts.1 += 1;
        } else if authorization.starts_with("AgentAssertion ") {
            counts.2 += 1;
        } else {
            assert_eq!(authorization, "Bearer e30.e30.c2ln");
            counts.0 += 1;
        }
    }
    assert_eq!(
        counts,
        (5, 5, 5),
        "ordinary, dual-header, assertion-only requests"
    );
    server.verify().await;
    Ok(())
}

#[tokio::test]
async fn history_notes_require_an_openai_provider_and_codex_backend_auth() -> TestResult {
    let codex_home = TempDir::new()?;
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    config.token_budget = Some(TokenBudgetConfig {
        use_history_notes_extension: true,
        ..TokenBudgetConfig::default()
    });

    for (provider, auth, thread_auth) in [
        (
            config.model_provider.clone(),
            CodexAuth::from_api_key("test-api-key"),
            None,
        ),
        (
            ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            None,
        ),
        (
            config.model_provider.clone(),
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            Some(CodexAuth::from_api_key("thread-api-key")),
        ),
    ] {
        config.model_provider = provider;
        let mut builder = ExtensionRegistryBuilder::<Config>::new();
        install(&mut builder, AuthManager::from_auth_for_testing(auth));
        let registry = builder.build();
        let session_store = ExtensionData::new("session-123");
        let thread_store = ExtensionData::new("thread-123");
        if let Some(auth) = thread_auth {
            thread_store.insert(ProviderAuthContext {
                credentials: AuthManager::from_auth_for_testing(auth),
                runtime_auth: None,
            });
        }

        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(ThreadStartInput {
                    config: &config,
                    session_source: &SessionSource::Cli,
                    persistent_thread_state_available: true,
                    environments: &[],
                    mcp_resource_client: None,
                    extension_metrics: None,
                    session_store: &session_store,
                    thread_store: &thread_store,
                })
                .await;
        }

        assert!(exposed_tools(&registry, &session_store, &thread_store).is_empty());
    }

    Ok(())
}

fn exposed_tools(
    registry: &ExtensionRegistry<Config>,
    session_store: &ExtensionData,
    thread_store: &ExtensionData,
) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
    registry
        .tool_contributors()
        .iter()
        .flat_map(|contributor| contributor.tools(session_store, thread_store))
        .collect()
}

fn tool_call(tool_name: ToolName, arguments: serde_json::Value) -> ToolCall<'static> {
    ToolCall {
        turn_id: "turn-1".to_string(),
        call_id: "call-read-file".to_string(),
        tool_name,
        model: "gpt-test".to_string(),
        codex_turn_metadata: None,
        truncation_policy: TruncationPolicy::Bytes(1024),
        source: ToolCallSource::Direct,
        conversation_history: ConversationHistory::default(),
        turn_item_emitter: Arc::new(NoopTurnItemEmitter),
        attachment_store: codex_core::passthrough_image_store(),
        ephemeral: false,
        environments: Vec::new(),
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}
