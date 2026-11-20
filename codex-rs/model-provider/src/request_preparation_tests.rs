use super::prepare_provider_request;
use crate::create_model_provider;
use crate::provider_uses_first_party_auth_path;
use base64::Engine;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn explicit_provider_token_survives_a_workspace_switch_during_setup() {
    let home = TempDir::new().unwrap();
    let claims = json!({"https://api.openai.com/auth": {"chatgpt_user_id": "user-a"}});
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    let token = format!("header.{payload}.signature");
    let initial = CodexAuth::from_external_chatgpt_tokens(
        &token,
        "workspace-a",
        /*chatgpt_plan_type*/ None,
    )
    .unwrap();
    let manager = AuthManager::from_auth_for_testing_with_home(initial, home.path().to_path_buf());
    let mut info = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
    info.experimental_bearer_token = Some("provider-token".into());
    let provider = create_model_provider(info, Some(manager.clone()));

    let mut attempts = 0;
    let resolved = prepare_provider_request(
        provider.as_ref(),
        provider_uses_first_party_auth_path(provider.info()),
        |_| {
            attempts += 1;
            async {
                let (_, resolved) = provider.request_provider_and_auth(/*scope*/ None).await?;
                codex_login::auth::login_with_chatgpt_auth_tokens(
                    home.path(),
                    &token,
                    "workspace-b",
                    /*chatgpt_plan_type*/ None,
                )?;
                manager.reload().await;
                Ok(resolved)
            }
        },
    )
    .await
    .unwrap();

    assert_eq!(
        manager.auth_cached().unwrap().get_account_id().as_deref(),
        Some("workspace-b")
    );
    assert_eq!(attempts, 1);
    let mut headers = http::HeaderMap::new();
    resolved.auth.add_auth_headers(&mut headers).unwrap();
    assert_eq!(
        resolved.credential.as_ref().and_then(CodexAuth::api_key),
        Some("provider-token")
    );
    assert_eq!(
        headers[http::header::AUTHORIZATION],
        "Bearer provider-token"
    );
}

#[tokio::test]
async fn resolved_request_distinguishes_credentials_from_unrelated_headers() {
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use codex_login::AuthHeaders;
    use http::HeaderMap;

    let header_auth = |name: &'static str, value: &'static str| {
        CodexAuth::Headers(AuthHeaders::new(HeaderMap::from_iter([(
            http::HeaderName::from_static(name),
            http::HeaderValue::from_static(value),
        )])))
    };
    for (name, auth, headers, expected) in [
        ("absent", None, vec![], false),
        (
            "bearer",
            Some(CodexAuth::from_api_key("test-key")),
            vec![],
            true,
        ),
        (
            "empty external headers",
            Some(CodexAuth::Headers(AuthHeaders::new(HeaderMap::new()))),
            vec![],
            false,
        ),
        (
            "external actor",
            Some(header_auth("x-openai-actor-authorization", "actor-token")),
            vec![],
            true,
        ),
        (
            "external API key",
            Some(header_auth("x-api-key", "provider-key")),
            vec![],
            true,
        ),
        (
            "unrelated external header",
            Some(header_auth("x-unrelated", "value")),
            vec![],
            false,
        ),
        (
            "account selector",
            Some(header_auth("chatgpt-account-id", "workspace")),
            vec![],
            false,
        ),
        (
            "empty external authorization",
            Some(header_auth("authorization", " ")),
            vec![],
            false,
        ),
        (
            "configured bearer",
            None,
            vec![("authorization", "Bearer provider-key")],
            true,
        ),
        (
            "configured actor",
            None,
            vec![("x-openai-actor-authorization", "actor-token")],
            true,
        ),
        (
            "configured API key",
            None,
            vec![("api-key", "provider-key")],
            true,
        ),
        (
            "empty configured authorization",
            None,
            vec![("authorization", " ")],
            false,
        ),
        (
            "unrelated configured header",
            None,
            vec![("x-unrelated", "value")],
            false,
        ),
    ] {
        let mut info = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
        info.http_headers = Some(
            headers
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string().into()))
                .collect(),
        );
        let provider = create_model_provider(info, auth.map(AuthManager::from_auth_for_testing));
        let setup = crate::resolve_request(
            provider.as_ref(),
            /*scope*/ None,
            /*routing*/ None,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .expect("local request preparation");
        assert_eq!(setup.has_authentication(), expected, "{name}");
    }
}
