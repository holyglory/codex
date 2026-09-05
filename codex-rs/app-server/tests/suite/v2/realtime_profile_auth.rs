use anyhow::Context;
use anyhow::Result;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::encode_id_token;
use app_test_support::write_models_cache;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_app_server_protocol::AccountProfileRemoveParams;
use codex_app_server_protocol::AccountProfileRemoveResponse;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadRealtimeClosedNotification;
use codex_app_server_protocol::ThreadRealtimeSdpNotification;
use codex_app_server_protocol::ThreadRealtimeStartParams;
use codex_app_server_protocol::ThreadRealtimeStartResponse;
use codex_app_server_protocol::ThreadRealtimeStopParams;
use codex_app_server_protocol::ThreadRealtimeStopResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::Feature;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ProfileAuthStorage;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use core_test_support::responses::WebSocketConnectionConfig;
use core_test_support::responses::start_websocket_server_with_headers;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
enum ProfileAuth {
    Chatgpt,
    ApiKey,
}

#[derive(Clone, Copy)]
enum ProfileSelection {
    Default,
    Reconnect,
    Pinned,
    ChangedDefault,
    Automatic,
    Unavailable,
}

#[test_case(ProfileAuth::Chatgpt, ProfileSelection::Default; "ChatGPT profile without legacy login")]
#[test_case(ProfileAuth::Chatgpt, ProfileSelection::Reconnect; "control reconnection retains selected credentials")]
#[test_case(ProfileAuth::ApiKey, ProfileSelection::Default; "API key profile without legacy login")]
#[test_case(ProfileAuth::Chatgpt, ProfileSelection::Pinned; "process pin overrides default")]
#[test_case(ProfileAuth::Chatgpt, ProfileSelection::ChangedDefault; "default changes after thread start")]
#[test_case(ProfileAuth::Chatgpt, ProfileSelection::Automatic; "automatic selection probes capacity")]
#[test_case(ProfileAuth::Chatgpt, ProfileSelection::Unavailable; "exhausted profiles never create a call")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_voice_uses_profile_auth(
    auth_kind: ProfileAuth,
    selection: ProfileSelection,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    let connection = WebSocketConnectionConfig {
        requests: Vec::new(),
        response_headers: Vec::new(),
        accept_delay: None,
        close_after_requests: false,
    };
    let connections = if matches!(selection, ProfileSelection::Reconnect) {
        vec![
            WebSocketConnectionConfig {
                close_after_requests: true,
                ..connection.clone()
            },
            connection,
        ]
    } else {
        vec![connection]
    };
    let sideband = start_websocket_server_with_headers(connections).await;
    let provider_path = match auth_kind {
        ProfileAuth::Chatgpt => "/backend-api/codex",
        ProfileAuth::ApiKey => "/v1",
    };
    MockResponsesConfig::new(&backend.uri())
        .with_provider_base_url(&format!("{}{provider_path}", backend.uri()))
        .with_provider_config("requires_openai_auth = true")
        .with_root_config(&format!(
            "chatgpt_base_url = {:?}\ncli_auth_credentials_store = \"file\"\nexperimental_realtime_ws_base_url = {:?}",
            backend.uri(),
            sideband.uri(),
        ))
        .enable_feature(Feature::RealtimeConversation)
        .write(codex_home.path())?;
    write_models_cache(codex_home.path())?;
    let alpha = persist_profile(codex_home.path(), "alpha", auth_kind)?;
    let beta = persist_profile(codex_home.path(), "beta", auth_kind)?;
    let mut registry = AccountRegistry {
        default_account_id: Some(alpha.id.clone()),
        accounts: vec![alpha.clone(), beta.clone()],
        ..Default::default()
    };
    registry.auto_selection.enabled = matches!(
        selection,
        ProfileSelection::Automatic | ProfileSelection::Unavailable
    );
    let store = RegistryStore::new(codex_home.path());
    store.create(&registry)?;
    assert!(!codex_home.path().join("auth.json").exists());

    for (alias, used_percent) in [("alpha", 100), ("beta", 10)] {
        let used_percent = match selection {
            ProfileSelection::Unavailable => 100,
            ProfileSelection::Default
            | ProfileSelection::Reconnect
            | ProfileSelection::Pinned
            | ProfileSelection::ChangedDefault
            | ProfileSelection::Automatic => used_percent,
        };
        Mock::given(method("GET"))
            .and(path("/api/codex/usage"))
            .and(header(
                "authorization",
                format!("Bearer synthetic-{alias}-access"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro",
                "rate_limit": {
                    "allowed": used_percent < 100,
                    "limit_reached": used_percent >= 100,
                    "primary_window": {
                        "used_percent": used_percent,
                        "limit_window_seconds": 3600,
                        "reset_after_seconds": 3600,
                        "reset_at": chrono::Utc::now().timestamp() + 3600
                    }
                }
            })))
            .mount(&backend)
            .await;
    }
    let selected_alias = match selection {
        ProfileSelection::Default | ProfileSelection::Reconnect | ProfileSelection::Unavailable => {
            "alpha"
        }
        ProfileSelection::Pinned
        | ProfileSelection::ChangedDefault
        | ProfileSelection::Automatic => "beta",
    };
    let call_path = match auth_kind {
        ProfileAuth::Chatgpt => "/backend-api/codex/realtime/calls",
        ProfileAuth::ApiKey => "/v1/live",
    };
    let expected_bearer = format!("Bearer synthetic-{selected_alias}-access");
    Mock::given(method("POST"))
        .and(path(call_path))
        .and(header("authorization", expected_bearer.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Location", "/v1/live/rtc_profile")
                .set_body_string("v=answer\r\n"),
        )
        .expect(u64::from(!matches!(
            selection,
            ProfileSelection::Unavailable
        )))
        .mount(&backend)
        .await;

    let mut builder = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            ("OPENAI_API_KEY", None),
        ]);
    if matches!(selection, ProfileSelection::Pinned) {
        builder = builder.with_args(&["--account", "beta"]);
    }
    let mut server = builder.build_initialized().await?;
    let thread_request = server
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let thread: ThreadStartResponse =
        timeout(TIMEOUT, server.read_response(thread_request)).await??;
    if matches!(selection, ProfileSelection::ChangedDefault) {
        registry = store.compare_and_swap(registry.generation, |registry| {
            registry.default_account_id = Some(beta.id.clone());
        })?;
    }
    let start: ThreadRealtimeStartParams = serde_json::from_value(json!({
        "threadId": thread.thread.id,
        "includeStartupContext": false,
        "outputModality": "audio",
        "prompt": "profile voice test",
        "transport": { "type": "webrtc", "sdp": "v=offer\r\n" },
        "version": "v3"
    }))?;
    let start_request = server.send_thread_realtime_start_request(start).await?;
    let _: ThreadRealtimeStartResponse =
        timeout(TIMEOUT, server.read_response(start_request)).await??;

    if matches!(selection, ProfileSelection::Unavailable) {
        let error: serde_json::Value =
            read_notification(&mut server, "thread/realtime/error").await?;
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("eligible"))
        );
        assert!(sideband.handshakes().is_empty());
    } else {
        let answer: ThreadRealtimeSdpNotification =
            read_notification(&mut server, "thread/realtime/sdp").await?;
        assert_eq!(
            answer,
            ThreadRealtimeSdpNotification {
                thread_id: thread.thread.id.clone(),
                sdp: "v=answer\r\n".to_string(),
            }
        );
        let expected_connections = if matches!(selection, ProfileSelection::Reconnect) {
            2
        } else {
            1
        };
        assert!(
            sideband
                .wait_for_handshakes(expected_connections, TIMEOUT)
                .await
        );
        let handshakes = sideband.handshakes();
        let expected_workspace = match auth_kind {
            ProfileAuth::Chatgpt => Some(format!("synthetic-{selected_alias}-workspace")),
            ProfileAuth::ApiKey => None,
        };
        for handshake in &handshakes {
            assert_eq!(
                (
                    handshake.uri(),
                    handshake.header("authorization"),
                    handshake.header("chatgpt-account-id")
                ),
                (
                    "/v1/live/rtc_profile",
                    Some(expected_bearer.clone()),
                    expected_workspace.clone()
                ),
            );
        }
        let requests = backend
            .received_requests()
            .await
            .context("backend requests")?;
        let call = requests
            .iter()
            .find(|request| request.method == "POST" && request.url.path() == call_path)
            .context("voice call request")?;
        assert_eq!(
            call.headers
                .get("chatgpt-account-id")
                .map(|value| value.to_str())
                .transpose()?,
            expected_workspace.as_deref()
        );
        match auth_kind {
            ProfileAuth::Chatgpt => {
                let body: serde_json::Value = serde_json::from_slice(&call.body)?;
                assert_eq!(body["sdp"], "v=offer\r\n");
            }
            ProfileAuth::ApiKey => {
                assert!(String::from_utf8_lossy(&call.body).contains("name=\"sdp\""))
            }
        }
        if matches!(selection, ProfileSelection::ChangedDefault) {
            store.compare_and_swap(registry.generation, |registry| {
                registry.default_account_id = Some(alpha.id.clone());
            })?;
            let removal_id = server
                .send_raw_request(
                    "accountProfile/remove",
                    Some(serde_json::to_value(AccountProfileRemoveParams {
                        account_id: beta.id.to_string(),
                    })?),
                )
                .await?;
            let removal = timeout(
                TIMEOUT,
                server.read_stream_until_error_message(RequestId::Integer(removal_id)),
            )
            .await??;
            assert_eq!(
                (removal.error.code, removal.error.message.as_str()),
                (-32602, "account profile is in use")
            );
        }
        let stop_request = server
            .send_thread_realtime_stop_request(ThreadRealtimeStopParams {
                thread_id: thread.thread.id.clone(),
            })
            .await?;
        let _: ThreadRealtimeStopResponse =
            timeout(TIMEOUT, server.read_response(stop_request)).await??;
        let closed: ThreadRealtimeClosedNotification =
            read_notification(&mut server, "thread/realtime/closed").await?;
        assert_eq!(closed.thread_id, thread.thread.id);
        if matches!(selection, ProfileSelection::ChangedDefault) {
            let removed: AccountProfileRemoveResponse = server
                .request(|request_id| ClientRequest::AccountProfileRemove {
                    request_id,
                    params: AccountProfileRemoveParams {
                        account_id: beta.id.to_string(),
                    },
                })
                .await?;
            assert_eq!(removed.account_id, beta.id.to_string());
        }
    }
    let requests = backend
        .received_requests()
        .await
        .context("backend requests")?;
    let expected_calls = usize::from(!matches!(selection, ProfileSelection::Unavailable));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST" && request.url.path() == call_path)
            .count(),
        expected_calls
    );
    if matches!(
        selection,
        ProfileSelection::Automatic | ProfileSelection::Unavailable
    ) {
        for alias in ["alpha", "beta"] {
            assert!(requests.iter().any(|request| {
                request.url.path() == "/api/codex/usage"
                    && request.headers.get("authorization").is_some_and(|value| {
                        value == format!("Bearer synthetic-{alias}-access").as_str()
                    })
            }));
        }
    }
    backend.verify().await;
    drop(server);
    sideband.shutdown().await;
    Ok(())
}

fn persist_profile(
    codex_home: &Path,
    alias: &str,
    auth_kind: ProfileAuth,
) -> Result<AccountMetadata> {
    let mut auth = AuthDotJson {
        auth_mode: None,
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    };
    let mode = match auth_kind {
        ProfileAuth::Chatgpt => {
            let workspace = format!("synthetic-{alias}-workspace");
            let claims = ChatGptIdTokenClaims::new()
                .email(format!("{alias}@example.test"))
                .plan_type("pro")
                .chatgpt_user_id(format!("synthetic-{alias}-user"))
                .chatgpt_account_id(workspace.clone());
            auth.tokens = Some(TokenData {
                id_token: parse_chatgpt_jwt_claims(&encode_id_token(&claims)?)?,
                access_token: format!("synthetic-{alias}-access"),
                refresh_token: format!("synthetic-{alias}-refresh"),
                account_id: Some(workspace),
            });
            auth.last_refresh = Some(chrono::Utc::now());
            AuthMode::Chatgpt
        }
        ProfileAuth::ApiKey => {
            auth.openai_api_key = Some(format!("synthetic-{alias}-access"));
            AuthMode::ApiKey
        }
    };
    auth.auth_mode = Some(mode);
    let mut metadata = AccountMetadata::new(alias.parse()?, mode, chrono::Utc::now());
    metadata.priority = u32::from(alias == "beta");
    let profile_metadata = auth.profile_metadata();
    metadata.email = profile_metadata.email;
    metadata.plan_type = profile_metadata.plan_type;
    ProfileAuthStorage::new(
        codex_home,
        metadata.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .save(&auth)?;
    Ok(metadata)
}

async fn read_notification<T: DeserializeOwned>(
    server: &mut TestAppServer,
    method: &str,
) -> Result<T> {
    let notification = timeout(
        TIMEOUT,
        server.read_stream_until_matching_notification(method, |notification| {
            notification.method == method || notification.method == "thread/realtime/error"
        }),
    )
    .await??;
    assert_eq!(
        notification.method, method,
        "unexpected realtime notification: {:?}",
        notification.params
    );
    Ok(serde_json::from_value(
        notification.params.context("notification params")?,
    )?)
}
