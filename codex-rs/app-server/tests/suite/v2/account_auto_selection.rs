use anyhow::Context;
use anyhow::Result;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::encode_id_token;
use app_test_support::write_models_cache;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::OpaqueServiceId;
use codex_account_registry::RegistryStore;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ProfileAuthStorage;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use codex_usage::ThreadId as UsageThreadId;
use codex_usage::UsageDetailKind;
use codex_usage::UsageDetailListQuery;
use codex_usage::UsageDetailRecord;
use codex_usage::UsagePageRequest;
use codex_usage::UsageStore;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::Times;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const RATE_LIMIT_PATH: &str = "/api/codex/usage";
const RESPONSES_PATH: &str = "/v1/responses";
const AUTOMATIC_SWITCH_WARNING: &str =
    "Automatically selected another eligible account for this turn.";
const UNKNOWN_CAPACITY_FATAL: &str =
    "Fatal error: automatic account selection failed: capacity is unknown";
const VERBOSE_RESPONSE_FIRST: &str =
    "The selected account completed the first detailed section without losing any text. ";
const VERBOSE_RESPONSE_SECOND: &str =
    "The second section remains visible after the account-switch notice is delivered.";
const FAILOVER_PROMPT: &str = "continue this turn after a clean account limit";
const FAILOVER_PLAN_CALL_ID: &str = "failover-plan-call";
const FAILOVER_BACKUP_PLAN_CALL_ID: &str = "failover-backup-plan-call";
const FAILOVER_FINAL_RESPONSE: &str =
    "The same logical turn continued on the backup account without replaying its tool.";
const EVENT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);

struct ManagedChatGptProfile {
    metadata: AccountMetadata,
    access_token: String,
    workspace_id: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_first_turn_refreshes_cli_profiles_and_selects_eligible_account() -> Result<()> {
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    write_test_config(codex_home.path(), &backend.uri())?;
    let [alpha, beta, gamma] = persist_cli_profile_set(codex_home.path())?;

    mount_observed_probe(
        &backend, &alpha, /*used_percent*/ 100, /*expected*/ 1,
    )
    .await;
    mount_failed_probe(&backend, &beta, /*expected*/ 2).await;
    mount_observed_probe(
        &backend, &gamma, /*used_percent*/ 10, /*expected*/ 1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path(RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(/*status*/ 426))
        .expect(1)
        .named("startup websocket prewarm falls back before the real turn")
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", gamma.access_token),
        ))
        .and(header("chatgpt-account-id", gamma.workspace_id.as_str()))
        .respond_with(responses::sse_response(responses::sse(vec![
            responses::ev_response_created("selected-gamma-response"),
            responses::ev_message_item_added("selected-gamma-message", ""),
            responses::ev_output_text_delta(VERBOSE_RESPONSE_FIRST),
            responses::ev_output_text_delta(VERBOSE_RESPONSE_SECOND),
            responses::ev_assistant_message(
                "selected-gamma-message",
                &format!("{VERBOSE_RESPONSE_FIRST}{VERBOSE_RESPONSE_SECOND}"),
            ),
            responses::ev_completed("selected-gamma-response"),
        ])))
        .expect(1)
        .named("model request under selected gamma profile")
        .mount(&backend)
        .await;

    let mut app_server = fresh_desktop_server(codex_home.path()).await?;
    let completed = start_first_turn(&mut app_server).await?;
    let requests = backend
        .received_requests()
        .await
        .context("read automatic-selection backend requests")?;
    let request_paths = requests
        .iter()
        .map(|request| request.url.path().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        completed.turn.status,
        TurnStatus::Completed,
        "backend request paths: {request_paths:?}"
    );
    assert_eq!(completed.turn.error, None);
    assert!(matches!(
        completed.turn.items.as_slice(),
        [ThreadItem::AgentMessage { text, .. }]
            if text == &format!("{VERBOSE_RESPONSE_FIRST}{VERBOSE_RESPONSE_SECOND}")
    ));
    let pending_methods = app_server.pending_notification_methods();
    let message_completed = pending_methods
        .iter()
        .position(|method| method == "item/completed")
        .context("completed verbose response notification")?;
    let switch_notice = pending_methods
        .iter()
        .rposition(|method| method == "warning")
        .context("automatic account switch warning notification")?;
    assert!(
        message_completed < switch_notice,
        "account switch warning arrived before the verbose response: {pending_methods:?}"
    );
    let warning_notification = timeout(
        EVENT_TIMEOUT,
        app_server.read_stream_until_matching_notification(
            "automatic account selection warning",
            |notification| {
                notification.method == "warning"
                    && notification.params.as_ref().is_some_and(|params| {
                        serde_json::from_value::<WarningNotification>(params.clone())
                            .is_ok_and(|warning| warning.message == AUTOMATIC_SWITCH_WARNING)
                    })
            },
        ),
    )
    .await??;
    let warning: WarningNotification = serde_json::from_value(
        warning_notification
            .params
            .context("automatic account selection warning has no parameters")?,
    )?;
    assert_eq!(
        warning.thread_id.as_deref(),
        Some(completed.thread_id.as_str())
    );
    assert_eq!(warning.message, AUTOMATIC_SWITCH_WARNING);

    backend.verify().await;
    assert_successful_probe_order(&requests, [&alpha, &beta, &gamma]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_turn_continues_on_backup_profile_after_clean_usage_limit() -> Result<()> {
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    write_test_config(codex_home.path(), &backend.uri())?;
    let [alpha, beta, gamma] = persist_cli_profile_set(codex_home.path())?;
    RegistryStore::new(codex_home.path())
        .compare_and_swap(/*expected_generation*/ 0, |registry| {
            registry.default_account_id = Some(beta.metadata.id.clone())
        })?;

    mount_observed_probe(
        &backend, &alpha, /*used_percent*/ 10, /*expected*/ 0,
    )
    .await;
    mount_observed_probe(
        &backend, &beta, /*used_percent*/ 10, /*expected*/ 1,
    )
    .await;
    mount_observed_probe(
        &backend, &gamma, /*used_percent*/ 10, /*expected*/ 1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path(RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(/*status*/ 426))
        .expect(1)
        .named("startup websocket prewarm falls back before failover turn")
        .mount(&backend)
        .await;

    let plan_arguments = json!({
        "plan": [{"step": "preserve completed tool work", "status": "completed"}]
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", beta.access_token),
        ))
        .and(header("chatgpt-account-id", beta.workspace_id.as_str()))
        .and(|request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(FAILOVER_PROMPT) && !body.contains(FAILOVER_PLAN_CALL_ID)
        })
        .respond_with(responses::sse_response(responses::sse(vec![
            responses::ev_response_created("failover-tool-response"),
            responses::ev_function_call(FAILOVER_PLAN_CALL_ID, "update_plan", &plan_arguments),
            responses::ev_completed("failover-tool-response"),
        ])))
        .expect(1)
        .named("tool response under initial profile")
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", beta.access_token),
        ))
        .and(header("chatgpt-account-id", beta.workspace_id.as_str()))
        .and(body_string_contains(FAILOVER_PLAN_CALL_ID))
        .respond_with(
            ResponseTemplate::new(/*status*/ 429)
                .insert_header("x-codex-primary-used-percent", "100")
                .insert_header("x-codex-primary-window-minutes", "300")
                .insert_header("x-codex-rate-limit-reached-type", "rate_limit_reached")
                .set_body_json(json!({
                    "error": {
                        "type": "usage_limit_reached",
                        "message": "initial profile reached its usage limit",
                        "resets_at": chrono::Utc::now().timestamp() + 3_600,
                        "plan_type": "pro"
                    }
                })),
        )
        .expect(1)
        .named("clean usage-limit rejection under initial profile")
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", gamma.access_token),
        ))
        .and(header("chatgpt-account-id", gamma.workspace_id.as_str()))
        .and(|request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(FAILOVER_PLAN_CALL_ID) && !body.contains(FAILOVER_BACKUP_PLAN_CALL_ID)
        })
        .respond_with(responses::sse_response(responses::sse(vec![
            responses::ev_response_created("backup-tool-response"),
            responses::ev_function_call(
                FAILOVER_BACKUP_PLAN_CALL_ID,
                "update_plan",
                &json!({
                    "plan": [{"step": "continue under backup profile", "status": "completed"}]
                })
                .to_string(),
            ),
            responses::ev_completed("backup-tool-response"),
        ])))
        .expect(1)
        .named("backup profile continues with one new tool")
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", gamma.access_token),
        ))
        .and(header("chatgpt-account-id", gamma.workspace_id.as_str()))
        .and(body_string_contains(FAILOVER_BACKUP_PLAN_CALL_ID))
        .respond_with(responses::sse_response(
            create_final_assistant_message_sse_response(FAILOVER_FINAL_RESPONSE)?,
        ))
        .expect(1)
        .named("same pending sampling request under backup profile")
        .mount(&backend)
        .await;

    let mut app_server = fresh_desktop_server(codex_home.path()).await?;
    let completed = start_turn(&mut app_server, FAILOVER_PROMPT).await?;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(completed.turn.error, None);
    assert!(completed.turn.items.iter().any(
        |item| matches!(item, ThreadItem::AgentMessage { text, .. } if text == FAILOVER_FINAL_RESPONSE)
    ));
    let pending_methods = app_server.pending_notification_methods();
    let message_completed = pending_methods
        .iter()
        .rposition(|method| method == "item/completed")
        .context("completed failover response notification")?;
    let switch_notice = pending_methods
        .iter()
        .rposition(|method| method == "warning")
        .context("failover warning notification")?;
    assert!(
        message_completed < switch_notice,
        "failover warning arrived before the complete response: {pending_methods:?}"
    );

    backend.verify().await;
    let requests = backend
        .received_requests()
        .await
        .context("failover requests")?;
    let model_requests = requests
        .iter()
        .filter(|request| {
            request.method == wiremock::http::Method::POST && request.url.path() == RESPONSES_PATH
        })
        .collect::<Vec<_>>();
    assert_eq!(model_requests.len(), 4);
    assert!(request_uses_profile(model_requests[0], &beta));
    assert!(request_uses_profile(model_requests[1], &beta));
    assert!(request_uses_profile(model_requests[2], &gamma));
    assert!(request_uses_profile(model_requests[3], &gamma));
    assert!(!String::from_utf8_lossy(&model_requests[0].body).contains(FAILOVER_PLAN_CALL_ID));
    assert!(String::from_utf8_lossy(&model_requests[1].body).contains(FAILOVER_PLAN_CALL_ID));
    assert!(String::from_utf8_lossy(&model_requests[2].body).contains(FAILOVER_PLAN_CALL_ID));
    assert!(
        String::from_utf8_lossy(&model_requests[3].body).contains(FAILOVER_BACKUP_PLAN_CALL_ID)
    );

    let store = UsageStore::open(codex_home.path()).await?;
    let details = store
        .list_details(
            UsageDetailKind::Operations,
            &UsageDetailListQuery {
                page: UsagePageRequest {
                    cursor: None,
                    limit: 50,
                },
                time_range: None,
                thread_id: Some(UsageThreadId::new(completed.thread_id.clone())?),
                repository_id: None,
                account_profile_ref: None,
            },
            |profile| profile.as_str().to_string(),
        )
        .await?;
    let mut operations = details
        .data
        .into_iter()
        .filter_map(|record| match record {
            UsageDetailRecord::Operation(operation)
                if operation.turn_id.as_deref() == Some(completed.turn.id.as_str()) =>
            {
                Some(*operation)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    operations.sort_by_key(|operation| operation.started_at_ms);
    let model_operations = operations
        .iter()
        .filter(|operation| operation.model_request.is_some())
        .collect::<Vec<_>>();
    assert_eq!(model_operations.len(), 4);
    assert_eq!(
        model_operations
            .iter()
            .map(|operation| operation.account.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some(beta.metadata.id.as_str()),
            Some(beta.metadata.id.as_str()),
            Some(gamma.metadata.id.as_str()),
            Some(gamma.metadata.id.as_str()),
        ]
    );
    assert_eq!(
        model_operations
            .iter()
            .map(|operation| {
                operation
                    .model_request
                    .as_ref()
                    .map(|request| request.attempt_number)
            })
            .collect::<Vec<_>>(),
        vec![Some(1), Some(1), Some(2), Some(1)]
    );
    assert_eq!(
        model_operations[2].retry_of_operation_id.as_deref(),
        Some(model_operations[1].id.as_str())
    );
    let plan_operations = operations
        .iter()
        .filter(|operation| {
            operation
                .tool
                .as_ref()
                .is_some_and(|tool| tool.safe_tool_name == "update_plan")
        })
        .collect::<Vec<_>>();
    assert_eq!(plan_operations.len(), 2);
    assert_eq!(
        plan_operations
            .iter()
            .map(|operation| operation.account.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some(beta.metadata.id.as_str()),
            Some(gamma.metadata.id.as_str()),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_turn_does_not_fail_over_after_response_stream_starts() -> Result<()> {
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    write_test_config(codex_home.path(), &backend.uri())?;
    let [alpha, beta, gamma] = persist_cli_profile_set(codex_home.path())?;
    RegistryStore::new(codex_home.path())
        .compare_and_swap(/*expected_generation*/ 0, |registry| {
            registry.default_account_id = Some(beta.metadata.id.clone())
        })?;

    mount_observed_probe(
        &backend, &alpha, /*used_percent*/ 10, /*expected*/ 0,
    )
    .await;
    mount_observed_probe(
        &backend, &beta, /*used_percent*/ 10, /*expected*/ 1,
    )
    .await;
    mount_observed_probe(
        &backend, &gamma, /*used_percent*/ 10, /*expected*/ 0,
    )
    .await;
    Mock::given(method("GET"))
        .and(path(RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(/*status*/ 426))
        .expect(1)
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", beta.access_token),
        ))
        .and(body_string_contains(FAILOVER_PROMPT))
        .respond_with(responses::sse_response(responses::sse(vec![
            responses::ev_response_created("partial-limit-response"),
            responses::ev_message_item_added("partial-limit-message", ""),
            responses::ev_output_text_delta("partial output must not be replayed"),
            json!({
                "type": "response.failed",
                "response": {
                    "id": "partial-limit-response",
                    "error": {
                        "code": "usage_limit_reached",
                        "message": "usage limit after response start"
                    }
                }
            }),
        ])))
        .expect(1)
        .named("partial response remains a visible failure")
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", gamma.access_token),
        ))
        .respond_with(responses::sse_response(
            create_final_assistant_message_sse_response("must not run")?,
        ))
        .expect(0)
        .named("no backup request after partial response")
        .mount(&backend)
        .await;

    let mut app_server = fresh_desktop_server(codex_home.path()).await?;
    let completed = start_turn(&mut app_server, FAILOVER_PROMPT).await?;
    assert_eq!(completed.turn.status, TurnStatus::Failed);
    assert!(completed.turn.error.is_some());
    backend.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_first_turn_fails_without_model_request_when_all_profile_probes_fail() -> Result<()>
{
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    write_test_config(codex_home.path(), &backend.uri())?;
    let profiles = persist_cli_profile_set(codex_home.path())?;
    for profile in &profiles {
        mount_failed_probe(&backend, profile, /*expected*/ 1..=2).await;
    }
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .respond_with(responses::sse_response(
            create_final_assistant_message_sse_response("must not run")?,
        ))
        .expect(0)
        .named("no model request before automatic selection succeeds")
        .mount(&backend)
        .await;

    let mut app_server = fresh_desktop_server(codex_home.path()).await?;
    let completed = start_first_turn(&mut app_server).await?;
    assert_eq!(completed.turn.status, TurnStatus::Failed);
    assert_eq!(
        completed
            .turn
            .error
            .as_ref()
            .map(|error| error.message.as_str()),
        Some(UNKNOWN_CAPACITY_FATAL)
    );

    backend.verify().await;
    let requests = backend
        .received_requests()
        .await
        .context("read failed automatic-selection backend requests")?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == RESPONSES_PATH)
            .count(),
        0
    );
    for profile in &profiles {
        let count = profile_probe_count(&requests, profile);
        assert!(
            (1..=2).contains(&count),
            "expected one bounded probe attempt per selection cycle for {}, got {count}",
            profile.metadata.alias
        );
    }
    Ok(())
}

fn write_test_config(codex_home: &Path, backend_uri: &str) -> Result<()> {
    MockResponsesConfig::new(backend_uri)
        .with_root_config(&format!(
            "chatgpt_base_url = \"{backend_uri}\"\ncli_auth_credentials_store = \"file\""
        ))
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = true")
        .write(codex_home)?;
    write_models_cache(codex_home)?;
    Ok(())
}

fn persist_cli_profile_set(codex_home: &Path) -> Result<[ManagedChatGptProfile; 3]> {
    let alpha = persist_managed_chatgpt_profile(codex_home, "alpha", /*priority*/ 0)?;
    let beta = persist_managed_chatgpt_profile(codex_home, "beta", /*priority*/ 2)?;
    let gamma = persist_managed_chatgpt_profile(codex_home, "gamma", /*priority*/ 1)?;
    let mut registry = AccountRegistry {
        default_account_id: Some(alpha.metadata.id.clone()),
        accounts: vec![
            alpha.metadata.clone(),
            beta.metadata.clone(),
            gamma.metadata.clone(),
        ],
        ..AccountRegistry::default()
    };
    registry.auto_selection.enabled = true;
    RegistryStore::new(codex_home).create(&registry)?;
    Ok([alpha, beta, gamma])
}

fn persist_managed_chatgpt_profile(
    codex_home: &Path,
    alias: &str,
    priority: u32,
) -> Result<ManagedChatGptProfile> {
    let access_token = format!("synthetic-{alias}-access");
    let workspace_id = format!("synthetic-{alias}-workspace");
    let claims = ChatGptIdTokenClaims::new()
        .email(format!("{alias}@example.test"))
        .plan_type("pro")
        .chatgpt_user_id(format!("synthetic-{alias}-user"))
        .chatgpt_account_id(workspace_id.clone());
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&encode_id_token(&claims)?)?,
            access_token: access_token.clone(),
            refresh_token: format!("synthetic-{alias}-refresh"),
            account_id: Some(workspace_id.clone()),
        }),
        last_refresh: Some(chrono::Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    };
    let profile_metadata = auth.profile_metadata();
    let mut metadata = AccountMetadata::new(
        alias.parse::<AccountAlias>()?,
        AuthMode::Chatgpt,
        chrono::Utc::now(),
    );
    metadata.priority = priority;
    metadata.email = profile_metadata.email;
    metadata.plan_type = profile_metadata.plan_type;
    metadata.service_account_id = profile_metadata
        .service_account_id
        .map(OpaqueServiceId::new)
        .transpose()?;
    metadata.service_workspace_id = profile_metadata
        .service_workspace_id
        .map(OpaqueServiceId::new)
        .transpose()?;
    ProfileAuthStorage::new(
        codex_home,
        metadata.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&auth)?;
    Ok(ManagedChatGptProfile {
        metadata,
        access_token,
        workspace_id,
    })
}

async fn mount_observed_probe<T: Into<Times>>(
    backend: &MockServer,
    profile: &ManagedChatGptProfile,
    used_percent: i32,
    expected: T,
) {
    let resets_at = chrono::Utc::now().timestamp() + 3_600;
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
                "allowed": used_percent < 100,
                "limit_reached": used_percent >= 100,
                "primary_window": {
                    "used_percent": used_percent,
                    "limit_window_seconds": 3600,
                    "reset_after_seconds": 3600,
                    "reset_at": resets_at
                }
            }
        })))
        .expect(expected)
        .named(format!("{} rate-limit probe", profile.metadata.alias))
        .mount(backend)
        .await;
}

async fn mount_failed_probe<T: Into<Times>>(
    backend: &MockServer,
    profile: &ManagedChatGptProfile,
    expected: T,
) {
    Mock::given(method("GET"))
        .and(path(RATE_LIMIT_PATH))
        .and(header(
            "authorization",
            format!("Bearer {}", profile.access_token),
        ))
        .and(header("chatgpt-account-id", profile.workspace_id.as_str()))
        .respond_with(ResponseTemplate::new(/*status*/ 503))
        .expect(expected)
        .named(format!(
            "{} failed rate-limit probe",
            profile.metadata.alias
        ))
        .mount(backend)
        .await;
}

async fn fresh_desktop_server(codex_home: &Path) -> Result<TestAppServer> {
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home)
        .with_env_overrides(&[
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ])
        .build()
        .await?;
    app_server
        .initialize_with_client_info(ClientInfo {
            name: "codex_desktop".to_string(),
            title: Some("Codex Desktop".to_string()),
            version: "0.1.0-test".to_string(),
        })
        .await?;
    Ok(app_server)
}

async fn start_first_turn(app_server: &mut TestAppServer) -> Result<TurnCompletedNotification> {
    start_turn(app_server, "use automatic account selection").await
}

async fn start_turn(
    app_server: &mut TestAppServer,
    prompt: &str,
) -> Result<TurnCompletedNotification> {
    let thread = app_server
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?
        .thread;
    timeout(
        EVENT_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await?
}

fn request_uses_profile(request: &Request, profile: &ManagedChatGptProfile) -> bool {
    let expected_authorization = format!("Bearer {}", profile.access_token);
    request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(expected_authorization.as_str())
        && request
            .headers
            .get("chatgpt-account-id")
            .and_then(|value| value.to_str().ok())
            == Some(profile.workspace_id.as_str())
}

fn assert_successful_probe_order(requests: &[Request], profiles: [&ManagedChatGptProfile; 3]) {
    let model_position = requests
        .iter()
        .position(|request| {
            request.method == wiremock::http::Method::POST && request.url.path() == RESPONSES_PATH
        })
        .expect("selected profile should make one model request");
    let probe_positions = requests
        .iter()
        .enumerate()
        .filter_map(|(index, request)| (request.url.path() == RATE_LIMIT_PATH).then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(probe_positions.len(), profiles.len() + 1);
    assert!(
        probe_positions
            .iter()
            .all(|probe_position| *probe_position < model_position),
        "every automatic-selection probe must finish before model work starts"
    );
    assert_eq!(profile_probe_count(requests, profiles[0]), 1);
    assert_eq!(profile_probe_count(requests, profiles[1]), 2);
    assert_eq!(profile_probe_count(requests, profiles[2]), 1);
}

fn profile_probe_count(requests: &[Request], profile: &ManagedChatGptProfile) -> usize {
    let expected_authorization = format!("Bearer {}", profile.access_token);
    requests
        .iter()
        .filter(|request| request.url.path() == RATE_LIMIT_PATH)
        .filter(|request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some(expected_authorization.as_str())
        })
        .count()
}
