use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::WorldStateContributionInput;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthManager;
use codex_login::AuthManagerLease;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthFuture;
use codex_login::ExternalAuthRefreshContext;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::GitAttributionExtension;
use super::policy::GitAttributionPolicy;
use super::policy::GitAttributionRetry;
use super::policy::auth_generation;
use super::policy::resolve_attribution_policy;

fn enterprise_auth_manager() -> Arc<AuthManager> {
    AuthManager::from_auth_for_testing(enterprise_auth("workspace-123"))
}

fn http_client_factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}

fn enterprise_auth(account_id: &str) -> CodexAuth {
    CodexAuth::from_external_chatgpt_tokens("e30.e30.c2ln", account_id, Some("enterprise"))
        .expect("fake ChatGPT auth should parse")
}

struct StaticExternalAuth(CodexAuth);

impl ExternalAuth for StaticExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        self.resolve()
    }
}

async fn set_auth(auth_manager: &AuthManager, account_id: &str) {
    auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(enterprise_auth(account_id))))
        .await
        .expect("auth refresh should succeed");
}

async fn attribution_state(
    extension: &GitAttributionExtension,
    thread_store: &ExtensionData,
    turn_store: &ExtensionData,
) -> Option<serde_json::Value> {
    let model_info = serde_json::from_value(serde_json::json!({
        "slug": "test-model",
        "display_name": "Test model",
        "supported_reasoning_levels": [],
        "shell_type": "disabled",
        "visibility": "none",
        "supported_in_api": false,
        "priority": 0,
        "support_verbosity": false,
        "truncation_policy": {"mode": "bytes", "limit": 1000},
        "experimental_supported_tools": [],
    }))
    .expect("unused model metadata");
    let session_store = ExtensionData::new("session");
    let step_store = ExtensionData::new("step");
    let sections = extension
        .contribute_world_state(WorldStateContributionInput {
            thread_id: "00000000-0000-0000-0000-000000000001"
                .try_into()
                .expect("thread id"),
            turn_id: "turn",
            model_info: &model_info,
            environments: &[],
            extension_metrics: None,
            session_store: &session_store,
            thread_store,
            turn_store,
            step_store: &step_store,
            previous_world_state: None,
        })
        .await;
    assert_eq!(sections.len(), 1);
    sections[0].render_diff(PreviousWorldStateSection::Absent).0
}

#[tokio::test]
async fn policy_resolution_recovers_after_unauthorized() {
    let server = MockServer::start().await;
    let request_count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/settings/user"))
        .respond_with({
            let request_count = request_count.clone();
            move |_request: &wiremock::Request| {
                if request_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(401)
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"commit_attribution_enabled": true}))
                }
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let auth_manager = enterprise_auth_manager();
    set_auth(auth_manager.as_ref(), "workspace-123").await;

    let policy = resolve_attribution_policy(
        &auth_manager,
        &format!("{}/backend-api", server.uri()),
        &http_client_factory(),
    )
    .await
    .expect("policy resolution should not time out")
    .expect("policy should resolve after auth recovery");

    assert!(policy.enabled);
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
    server.verify().await;
}

#[tokio::test]
async fn policy_resolution_retries_after_auth_refresh() {
    let server = MockServer::start().await;
    let request_started = Arc::new(Notify::new());
    let request_count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/settings/user"))
        .respond_with({
            let request_started = request_started.clone();
            let request_count = request_count.clone();
            move |_request: &wiremock::Request| match request_count.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    request_started.notify_one();
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(100))
                        .set_body_json(serde_json::json!({
                            "commit_attribution_enabled": true,
                        }))
                }
                1 => ResponseTemplate::new(401),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "commit_attribution_enabled": true,
                })),
            }
        })
        .expect(3)
        .mount(&server)
        .await;
    let auth_manager = enterprise_auth_manager();
    let resolve = tokio::spawn({
        let auth_manager = auth_manager.clone();
        let base_url = format!("{}/backend-api", server.uri());
        async move {
            resolve_attribution_policy(&auth_manager, &base_url, &http_client_factory())
                .await
                .ok()
                .flatten()
        }
    });

    tokio::time::timeout(Duration::from_secs(5), request_started.notified())
        .await
        .expect("first settings request should start");
    set_auth(auth_manager.as_ref(), "workspace-456").await;

    assert!(
        resolve
            .await
            .expect("policy task should complete")
            .expect("policy should resolve after refresh")
            .enabled
    );
    server.verify().await;
}

#[tokio::test]
async fn cached_policy_and_retry_follow_the_selected_auth_manager() {
    #[derive(Clone, Copy, Debug)]
    enum Cached {
        ThreadPolicy,
        TurnPolicy,
        TimeoutRetry,
    }
    for cached in [
        Cached::ThreadPolicy,
        Cached::TurnPolicy,
        Cached::TimeoutRetry,
    ] {
        let server = MockServer::start().await;
        let first_response = match cached {
            Cached::ThreadPolicy => ResponseTemplate::new(/*s*/ 200)
                .set_body_json(serde_json::json!({"commit_attribution_enabled": false})),
            Cached::TurnPolicy => ResponseTemplate::new(/*s*/ 400),
            Cached::TimeoutRetry => ResponseTemplate::new(/*s*/ 200)
                .set_delay(Duration::from_secs(/*secs*/ 1))
                .set_body_json(serde_json::json!({"commit_attribution_enabled": false})),
        };
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/settings/user"))
            .and(header("chatgpt-account-id", "workspace-a"))
            .respond_with(first_response)
            .expect(/*r*/ 1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/settings/user"))
            .and(header("chatgpt-account-id", "workspace-b"))
            .respond_with(
                ResponseTemplate::new(/*s*/ 200).set_body_json(serde_json::json!({
                    "commit_attribution_enabled": true,
                })),
            )
            .expect(/*r*/ 1)
            .mount(&server)
            .await;
        let first = AuthManager::from_auth_for_testing(enterprise_auth("workspace-a"));
        let first_owner = Arc::downgrade(&first);
        let selected = AuthManager::from_auth_for_testing(enterprise_auth("workspace-b"));
        assert_eq!(auth_generation(&first), auth_generation(&selected));
        let extension = GitAttributionExtension {
            auth_manager: Arc::clone(&first),
            base_url: format!("{}/backend-api", server.uri()),
            http_client_factory: http_client_factory(),
        };
        let thread_store = ExtensionData::new("thread");
        let turn_store = ExtensionData::new("turn");
        turn_store.insert(AuthManagerLease::legacy(Arc::clone(&first)));
        assert_eq!(
            attribution_state(&extension, &thread_store, &turn_store).await,
            Some(serde_json::json!(false)),
            "first account {cached:?}"
        );
        match cached {
            Cached::ThreadPolicy => assert!(thread_store.get::<GitAttributionPolicy>().is_some()),
            Cached::TurnPolicy => assert!(turn_store.get::<GitAttributionPolicy>().is_some()),
            Cached::TimeoutRetry => assert!(thread_store.get::<GitAttributionRetry>().is_some()),
        }
        assert_eq!(
            attribution_state(&extension, &thread_store, &turn_store).await,
            Some(serde_json::json!(false)),
            "the same owner keeps its cached result or retry delay"
        );

        // Preserve the extension values and unexpired retry while replacing the lease.
        turn_store.insert(AuthManagerLease::legacy(Arc::clone(&selected)));
        assert_eq!(auth_generation(&first), auth_generation(&selected));
        for _ in 0..2 {
            assert_eq!(
                attribution_state(&extension, &thread_store, &turn_store).await,
                Some(serde_json::json!(true)),
                "the selected owner resolves and caches its own policy despite {cached:?}"
            );
        }
        server.verify().await;
        drop(extension);
        drop(first);
        assert!(
            first_owner.upgrade().is_none(),
            "cached state must not keep the previous credential owner alive"
        );
    }
}
