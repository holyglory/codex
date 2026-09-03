use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::DEFAULT_ACCOUNT_PRIORITY;
use codex_account_registry::RegistryStore;
use codex_config::ManagedAuthPolicy;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use serde_json::json;
use serial_test::serial;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;
use tokio::sync::Notify;

use super::*;
use crate::auth::AuthDotJson;
use crate::auth::AuthKeyringBackendKind;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

fn auth(marker: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(format!("{marker}-key")),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn chatgpt_auth(marker: &str) -> AuthDotJson {
    let payload = json!({
        "email": format!("{marker}@example.com"),
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": format!("{marker}-workspace")
        }
    });
    let jwt = format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload")),
        URL_SAFE_NO_PAD.encode(b"signature")
    );
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&jwt).expect("token"),
            access_token: format!("{marker}-access"),
            refresh_token: format!("{marker}-refresh"),
            account_id: Some(format!("{marker}-workspace")),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
        bedrock_access_keys: None,
    }
}

fn seed_chatgpt_account(codex_home: &std::path::Path, alias: &str) -> AccountMetadata {
    let account = AccountMetadata::new(
        alias.parse::<AccountAlias>().expect("alias"),
        AuthMode::Chatgpt,
        Utc::now(),
    );
    ProfileAuthStorage::new(
        codex_home,
        account.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("profile")
    .save(&chatgpt_auth(alias))
    .expect("auth");
    account
}

fn limits(used_percent: f64) -> codex_protocol::protocol::RateLimitSnapshot {
    codex_protocol::protocol::RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(codex_protocol::protocol::RateLimitWindow {
            used_percent,
            window_minutes: Some(300),
            resets_at: Some(Utc::now().timestamp() + 300),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn usage_limit_failover_excludes_rejected_profiles_and_respects_process_pins() {
    let home = tempdir().expect("home");
    let mut account_a = seed_chatgpt_account(home.path(), "a");
    let mut account_b = seed_chatgpt_account(home.path(), "b");
    let mut account_c = seed_chatgpt_account(home.path(), "c");
    account_a.priority = 3;
    account_b.priority = 2;
    account_c.priority = 1;
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone(), account_c.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let observed_at = Utc::now().timestamp();
    for account in [&account_a, &account_b, &account_c] {
        shared
            .record_rate_limits(account.id.clone(), observed_at, vec![limits(10.0)])
            .expect("record eligible limits");
    }

    let first = shared
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await
        .expect("select first profile")
        .expect("configured profile router");
    assert_eq!(first.account_id(), &account_a.id);

    let mut excluded = HashSet::from([account_a.id.clone()]);
    let second = shared
        .lease_for_usage_limit_failover_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            &excluded,
            |_| async { None },
        )
        .await
        .expect("select second profile")
        .expect("eligible failover profile");
    assert_eq!(second.account_id(), &account_b.id);
    assert!(second.automatic_switched());

    excluded.insert(account_b.id.clone());
    let third = shared
        .lease_for_usage_limit_failover_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            &excluded,
            |_| async { None },
        )
        .await
        .expect("select third profile")
        .expect("last eligible failover profile");
    assert_eq!(third.account_id(), &account_c.id);

    excluded.insert(account_c.id.clone());
    assert!(
        shared
            .lease_for_usage_limit_failover_with_external_auth_and_probe(
                RouterExternalAuthState::default(),
                &excluded,
                |_| async { None },
            )
            .await
            .expect("exhausted failover pool")
            .is_none()
    );

    let pinned = SharedProfileAuthRouter::new_pinned(
        config(home.path()),
        account_a.alias.as_str().to_string(),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    assert!(
        pinned
            .lease_for_usage_limit_failover_with_external_auth_and_probe(
                RouterExternalAuthState::default(),
                &HashSet::from([account_a.id]),
                |_| async { None },
            )
            .await
            .expect("pinned failover refusal")
            .is_none()
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn shared_auto_selection_uses_cache_and_retains_selected_account() {
    let home = tempdir().expect("home");
    let account_a = seed_chatgpt_account(home.path(), "a");
    let account_b = seed_chatgpt_account(home.path(), "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let observed_at = Utc::now().timestamp();
    shared
        .record_rate_limits(
            account_a.id.clone(),
            observed_at,
            vec![limits(/*used_percent*/ 100.0)],
        )
        .expect("reached limits");
    shared
        .record_rate_limits(
            account_b.id.clone(),
            observed_at,
            vec![limits(/*used_percent*/ 10.0)],
        )
        .expect("eligible limits");

    let first = shared
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await
        .expect("selection")
        .expect("lease");
    assert_eq!(first.account_id(), &account_b.id);
    assert!(first.automatic_switched());
    drop(first);
    let second = shared
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await
        .expect("selection")
        .expect("lease");
    assert_eq!(second.account_id(), &account_b.id);
    assert!(!second.automatic_switched());
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn shared_auto_selection_fails_closed_for_unknown_capacity() {
    let home = tempdir().expect("home");
    let account_a = seed_chatgpt_account(home.path(), "a");
    let account_b = seed_chatgpt_account(home.path(), "b");
    seed_registry(home.path(), vec![account_a, account_b], /*default*/ 0);
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let error = shared
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await
        .expect_err("unknown capacity must not be probed implicitly");
    assert_eq!(
        error.safe_message(),
        "automatic account selection failed: capacity is unknown"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_refreshes_stale_current_then_candidates_in_descending_priority() {
    let home = tempdir().expect("home");
    let mut account_a = seed_chatgpt_account(home.path(), "a");
    let mut account_b = seed_chatgpt_account(home.path(), "b");
    let mut account_c = seed_chatgpt_account(home.path(), "c");
    account_a.priority = 500;
    account_b.priority = DEFAULT_ACCOUNT_PRIORITY;
    account_c.priority = 1;
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone(), account_c.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    shared
        .record_rate_limits(
            account_a.id.clone(),
            Utc::now().timestamp() - MAX_LIMIT_AGE_SECONDS - 1,
            vec![limits(/*used_percent*/ 10.0)],
        )
        .expect("stale current limits");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let current_id = account_a.id.clone();
    let higher_id = account_b.id.clone();
    let calls_for_probe = Arc::clone(&calls);

    let lease = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |lease| {
                let calls = Arc::clone(&calls_for_probe);
                let current_id = current_id.clone();
                let higher_id = higher_id.clone();
                async move {
                    let account_id = lease.account_id().expect("profile lease").clone();
                    calls.lock().expect("calls").push(account_id.clone());
                    Some(vec![limits(
                        if account_id == current_id || account_id == higher_id {
                            100.0
                        } else {
                            10.0
                        },
                    )])
                }
            },
        )
        .await
        .expect("probed selection")
        .expect("lease");

    assert_eq!(lease.account_id(), &account_c.id);
    assert!(lease.automatic_switched());
    assert_eq!(
        *calls.lock().expect("calls"),
        vec![account_a.id, account_b.id, account_c.id]
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_switches_from_fresh_lower_priority_to_higher_priority() {
    let home = tempdir().expect("home");
    let mut current = seed_chatgpt_account(home.path(), "current");
    let mut higher = seed_chatgpt_account(home.path(), "higher");
    current.priority = 1;
    higher.priority = DEFAULT_ACCOUNT_PRIORITY;
    seed_registry(
        home.path(),
        vec![current.clone(), higher.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    shared
        .record_rate_limits(
            current.id.clone(),
            Utc::now().timestamp(),
            vec![limits(/*used_percent*/ 10.0)],
        )
        .expect("fresh current limits");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_probe = Arc::clone(&calls);

    let lease = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |lease| {
                let calls = Arc::clone(&calls_for_probe);
                async move {
                    calls
                        .lock()
                        .expect("calls")
                        .push(lease.account_id().expect("profile lease").clone());
                    Some(vec![limits(/*used_percent*/ 10.0)])
                }
            },
        )
        .await
        .expect("probed selection")
        .expect("lease");

    assert_eq!(lease.account_id(), &higher.id);
    assert!(lease.automatic_switched());
    assert_eq!(*calls.lock().expect("calls"), vec![higher.id]);
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_preview_preserves_the_first_real_turn_switch() {
    let home = tempdir().expect("home");
    let account_a = seed_chatgpt_account(home.path(), "a");
    let account_b = seed_chatgpt_account(home.path(), "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let default_id = account_a.id.clone();

    let preview = shared
        .lease_for_startup_prewarm_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |lease| {
                let default_id = default_id.clone();
                async move {
                    let account_id = lease.account_id().expect("profile lease");
                    Some(vec![limits(if account_id == &default_id {
                        100.0
                    } else {
                        10.0
                    })])
                }
            },
        )
        .await
        .expect("preview selection")
        .expect("preview lease");
    assert_eq!(preview.account_id(), &account_b.id);
    assert!(preview.automatic_switched());
    drop(preview);

    let first_turn = shared
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await
        .expect("first real selection")
        .expect("first real lease");
    assert_eq!(first_turn.account_id(), &account_b.id);
    assert!(first_turn.automatic_switched());
    drop(first_turn);

    let second_turn = shared
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await
        .expect("second real selection")
        .expect("second real lease");
    assert_eq!(second_turn.account_id(), &account_b.id);
    assert!(!second_turn.automatic_switched());
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_retains_fresh_current_with_an_equal_priority_peer() {
    let home = tempdir().expect("home");
    let account_a = seed_chatgpt_account(home.path(), "a");
    let account_b = seed_chatgpt_account(home.path(), "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    shared
        .record_rate_limits(
            account_a.id.clone(),
            Utc::now().timestamp(),
            vec![limits(/*used_percent*/ 10.0)],
        )
        .expect("fresh current limits");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_probe = Arc::clone(&calls);

    let lease = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |_lease| {
                calls_for_probe.fetch_add(1, Ordering::SeqCst);
                async { None::<Vec<codex_protocol::protocol::RateLimitSnapshot>> }
            },
        )
        .await
        .expect("selection")
        .expect("lease");

    assert_eq!(lease.account_id(), &account_a.id);
    assert!(!lease.automatic_switched());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn concurrent_selection_probes_are_coalesced() {
    let home = tempdir().expect("home");
    let account = seed_chatgpt_account(home.path(), "a");
    seed_registry(home.path(), vec![account.clone()], /*default*/ 0);
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let first_router = shared.clone();
    let first_calls = Arc::clone(&calls);
    let first_entered = Arc::clone(&entered);
    let first_release = Arc::clone(&release);
    let first = tokio::spawn(async move {
        first_router
            .lease_for_turn_with_external_auth_and_probe(
                RouterExternalAuthState::default(),
                move |_lease| {
                    let calls = Arc::clone(&first_calls);
                    let entered = Arc::clone(&first_entered);
                    let release = Arc::clone(&first_release);
                    async move {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        if call == 0 {
                            entered.notify_one();
                            release.notified().await;
                        }
                        Some(vec![limits(/*used_percent*/ 10.0)])
                    }
                },
            )
            .await
    });
    entered.notified().await;

    let second_router = shared.clone();
    let second_calls = Arc::clone(&calls);
    let second = tokio::spawn(async move {
        second_router
            .lease_for_turn_with_external_auth_and_probe(
                RouterExternalAuthState::default(),
                move |_lease| {
                    let calls = Arc::clone(&second_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Some(vec![limits(/*used_percent*/ 10.0)])
                    }
                },
            )
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();

    let first_lease = first
        .await
        .expect("first task")
        .expect("first selection")
        .expect("first lease");
    let second_lease = second
        .await
        .expect("second task")
        .expect("second selection")
        .expect("second lease");
    assert_eq!(first_lease.account_id(), &account.id);
    assert_eq!(second_lease.account_id(), &account.id);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_skips_excluded_auth_modes() {
    let home = tempdir().expect("home");
    let (api_key, _) = seed_account(home.path(), "api-key", "api-key");
    let chatgpt = seed_chatgpt_account(home.path(), "chatgpt");
    seed_registry(
        home.path(),
        vec![api_key.clone(), chatgpt.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_probe = Arc::clone(&calls);

    let lease = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |lease| {
                let calls = Arc::clone(&calls_for_probe);
                async move {
                    calls
                        .lock()
                        .expect("calls")
                        .push(lease.account_id().expect("profile lease").clone());
                    Some(vec![limits(/*used_percent*/ 10.0)])
                }
            },
        )
        .await
        .expect("selection")
        .expect("lease");

    assert_eq!(lease.account_id(), &chatgpt.id);
    assert_eq!(*calls.lock().expect("calls"), vec![chatgpt.id]);
    assert_ne!(lease.account_id(), &api_key.id);
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_failure_remains_content_free_and_attempts_each_profile_once() {
    let home = tempdir().expect("home");
    let account_a = seed_chatgpt_account(home.path(), "a");
    let account_b = seed_chatgpt_account(home.path(), "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_probe = Arc::clone(&calls);

    let error = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |lease| {
                let calls = Arc::clone(&calls_for_probe);
                async move {
                    calls
                        .lock()
                        .expect("calls")
                        .push(lease.account_id().expect("profile lease").clone());
                    None
                }
            },
        )
        .await
        .expect_err("unknown capacity must remain unavailable");

    assert_eq!(
        error.safe_message(),
        "automatic account selection failed: capacity is unknown"
    );
    assert_eq!(
        *calls.lock().expect("calls"),
        vec![account_a.id, account_b.id]
    );
}

fn config(codex_home: &std::path::Path) -> AuthConfig {
    AuthConfig {
        codex_home: codex_home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: AuthKeyringBackendKind::Direct,
        forced_login_method: None,
        chatgpt_base_url: None,
        forced_chatgpt_workspace_id: None,
        managed_auth_policy: ManagedAuthPolicy::default(),
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    }
}

fn router_config(codex_home: &std::path::Path) -> ProfileAuthRouterConfig {
    ProfileAuthRouterConfig {
        auth_config: config(codex_home),
        process_pin: None,
        external_auth: RouterExternalAuthState::default(),
    }
}

fn seed_account(
    codex_home: &std::path::Path,
    alias: &str,
    marker: &str,
) -> (AccountMetadata, AuthDotJson) {
    let account = AccountMetadata::new(
        alias.parse::<AccountAlias>().expect("valid alias"),
        AuthMode::ApiKey,
        Utc::now(),
    );
    let auth = auth(marker);
    ProfileAuthStorage::new(
        codex_home,
        account.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("open profile")
    .save(&auth)
    .expect("save profile auth");
    (account, auth)
}

fn seed_registry(codex_home: &std::path::Path, accounts: Vec<AccountMetadata>, default: usize) {
    let registry = AccountRegistry {
        default_account_id: Some(accounts[default].id.clone()),
        accounts,
        ..AccountRegistry::default()
    };
    RegistryStore::new(codex_home)
        .create(&registry)
        .expect("create registry");
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn active_lease_survives_global_activation_and_same_manager_observes_refresh() {
    let home = tempdir().expect("temporary home");
    let (account_a, _) = seed_account(home.path(), "a", "a");
    let (account_b, _) = seed_account(home.path(), "b", "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    let router = ProfileAuthRouter::open(router_config(home.path()))
        .await
        .expect("open router");
    let lease_a = router.lease_for_turn().await.expect("lease account a");

    assert_eq!(
        router
            .activate_default(&account_b.id, /*expected_generation*/ 0)
            .await
            .expect("activate b"),
        1
    );
    let lease_b = router.lease_for_turn().await.expect("lease account b");
    assert_eq!(lease_a.account_id(), &account_a.id);
    assert_eq!(lease_b.account_id(), &account_b.id);

    ProfileAuthStorage::new(
        home.path(),
        account_a.id.clone(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("open account a")
    .save(&auth("a-refreshed"))
    .expect("refresh account a storage");
    lease_a.auth_manager().reload().await;
    assert_eq!(
        lease_a
            .auth_manager()
            .auth_cached()
            .and_then(|auth| auth.api_key().map(str::to_string)),
        Some("a-refreshed-key".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn process_pin_resists_global_activation_and_blocks_removal() {
    let home = tempdir().expect("temporary home");
    let (account_a, _) = seed_account(home.path(), "a", "a");
    let (account_b, _) = seed_account(home.path(), "b", "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    let mut router_settings = router_config(home.path());
    router_settings.process_pin = Some(account_a.id.clone());
    let router = ProfileAuthRouter::open(router_settings)
        .await
        .expect("open pinned router");
    router
        .activate_default(&account_b.id, /*expected_generation*/ 0)
        .await
        .expect("activate b");

    let lease = router.lease_for_turn().await.expect("lease pinned account");
    assert_eq!(lease.account_id(), &account_a.id);
    assert!(matches!(
        router.check_removal_allowed(&account_a.id),
        Err(ProfileAuthRouterError::AccountInUse)
    ));
    assert!(router.check_removal_allowed(&account_b.id).is_ok());
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn unpinned_active_lease_blocks_removal_only_until_drop() {
    let home = tempdir().expect("temporary home");
    let (account, _) = seed_account(home.path(), "a", "a");
    seed_registry(home.path(), vec![account.clone()], /*default*/ 0);
    let router = ProfileAuthRouter::open(router_config(home.path()))
        .await
        .expect("open router");
    let lease = router.lease_for_turn().await.expect("lease account");
    assert!(matches!(
        router.check_removal_allowed(&account.id),
        Err(ProfileAuthRouterError::AccountInUse)
    ));
    drop(lease);
    assert!(router.check_removal_allowed(&account.id).is_ok());
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn persistent_removal_respects_cross_process_lease_lock_and_selects_fallback() {
    let home = tempdir().expect("temporary home");
    let (account_a, _) = seed_account(home.path(), "a", "a");
    let (mut account_b, _) = seed_account(home.path(), "b", "b");
    account_b.priority = 1;
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    let router = ProfileAuthRouter::open(router_config(home.path()))
        .await
        .expect("open router");
    let lease = router
        .lease_for_turn()
        .await
        .expect("lease default account");

    assert!(matches!(
        ProfileAuthRouter::remove_persistent_account(
            &config(home.path()),
            &account_a.id,
            /*expected_generation*/ 0
        ),
        Err(ProfileAuthRouterError::AccountInUse)
    ));
    drop(lease);

    let outcome = ProfileAuthRouter::remove_persistent_account(
        &config(home.path()),
        &account_a.id,
        /*expected_generation*/ 0,
    )
    .expect("remove profile after lease ends");
    assert_eq!(
        outcome,
        ProfileRemovalOutcome {
            generation: 1,
            default_account_id: Some(account_b.id.clone()),
            credentials_removed: true,
        }
    );
    assert_eq!(
        RegistryStore::new(home.path())
            .read()
            .expect("read updated registry")
            .accounts,
        vec![account_b]
    );
    assert_eq!(
        ProfileAuthStorage::new(
            home.path(),
            account_a.id,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )
        .expect("open removed profile")
        .load()
        .expect("check removed credentials"),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_migrates_once_and_does_not_reimport() {
    let home = tempdir().expect("temporary home");
    crate::auth::save_auth(
        home.path(),
        &auth("legacy"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("save legacy auth");
    let first = ProfileAuthRouter::open(router_config(home.path()))
        .await
        .expect("open migrated router");
    let first_lease = first
        .lease_for_turn()
        .await
        .expect("lease migrated account");
    crate::auth::save_auth(
        home.path(),
        &auth("new-legacy"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("recreate legacy auth");
    let second = ProfileAuthRouter::open(router_config(home.path()))
        .await
        .expect("reopen router");
    let second_lease = second
        .lease_for_turn()
        .await
        .expect("lease existing profile");
    assert_eq!(first_lease.account_id(), second_lease.account_id());
    assert_eq!(
        second_lease
            .auth_manager()
            .auth_cached()
            .and_then(|auth| auth.api_key().map(str::to_string)),
        Some("legacy-key".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn every_external_source_conflicts_without_exposing_values() {
    let home = tempdir().expect("temporary home");
    let (account, _) = seed_account(home.path(), "a", "a");
    seed_registry(home.path(), vec![account.clone()], /*default*/ 0);
    for (state, source) in [
        (
            RouterExternalAuthState {
                codex_api_key_environment: true,
                ..Default::default()
            },
            ExternalAuthConflictSource::CodexApiKeyEnvironment,
        ),
        (
            RouterExternalAuthState {
                codex_access_token_environment: true,
                ..Default::default()
            },
            ExternalAuthConflictSource::CodexAccessTokenEnvironment,
        ),
        (
            RouterExternalAuthState {
                external_chatgpt: true,
                ..Default::default()
            },
            ExternalAuthConflictSource::ExternalChatgpt,
        ),
        (
            RouterExternalAuthState {
                workload_identity: true,
                ..Default::default()
            },
            ExternalAuthConflictSource::WorkloadIdentity,
        ),
        (
            RouterExternalAuthState {
                header_or_host: true,
                ..Default::default()
            },
            ExternalAuthConflictSource::HeaderOrHost,
        ),
    ] {
        let mut settings = router_config(home.path());
        settings.process_pin = Some(account.id.clone());
        settings.external_auth = state;
        let error = ProfileAuthRouter::open(settings)
            .await
            .expect_err("external source must conflict with pin");
        assert!(
            matches!(error, ProfileAuthRouterError::PinConflict { conflict } if conflict == source)
        );
        assert!(!error.to_string().contains("secret"));
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn shared_process_pin_survives_global_default_changes_across_turns() {
    let home = tempdir().expect("temporary home");
    let (account_a, _) = seed_account(home.path(), "a", "a");
    let (account_b, _) = seed_account(home.path(), "b", "b");
    seed_registry(
        home.path(),
        vec![account_a.clone(), account_b.clone()],
        /*default*/ 0,
    );
    let shared = SharedProfileAuthRouter::new_pinned(
        config(home.path()),
        "a".to_string(),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );

    let first = shared
        .lease_for_turn_if_configured()
        .await
        .expect("first pinned lease")
        .expect("registry configured");
    RegistryStore::new(home.path())
        .compare_and_swap(/*expected_generation*/ 0, |registry| {
            registry.default_account_id = Some(account_b.id.clone());
        })
        .expect("change global default");
    let second = shared
        .lease_for_turn_if_configured()
        .await
        .expect("second pinned lease")
        .expect("registry configured");

    assert_eq!(first.account_id(), &account_a.id);
    assert_eq!(second.account_id(), &account_a.id);
    assert_eq!(
        RegistryStore::new(home.path())
            .read()
            .expect("read registry")
            .default_account_id,
        Some(account_b.id)
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn shared_process_pin_rejects_safe_external_category_before_lease() {
    let home = tempdir().expect("temporary home");
    let (account, _) = seed_account(home.path(), "a", "a");
    seed_registry(home.path(), vec![account], /*default*/ 0);
    let shared = SharedProfileAuthRouter::new_pinned(
        config(home.path()),
        "a".to_string(),
        RouterExternalAuthState {
            header_or_host: true,
            ..Default::default()
        },
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );

    let error = shared
        .lease_for_turn_if_configured()
        .await
        .expect_err("external auth must reject pin");
    assert!(matches!(
        error,
        ProfileAuthRouterError::PinConflict {
            conflict: ExternalAuthConflictSource::HeaderOrHost
        }
    ));
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn operation_lease_uses_upstream_auth_for_an_empty_registry() {
    let home = tempdir().expect("temporary home");
    RegistryStore::new(home.path())
        .create(&AccountRegistry::default())
        .expect("create empty registry");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );

    let lease = shared
        .lease_for_operation()
        .await
        .expect("legacy operation lease");

    assert_eq!(
        lease
            .auth_manager()
            .auth_cached()
            .expect("upstream auth")
            .get_token()
            .expect("upstream token"),
        "upstream"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn shared_process_pin_classifies_unknown_disabled_and_logged_out() {
    let upstream = || AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream"));
    let missing_home = tempdir().expect("temporary missing home");
    let missing = SharedProfileAuthRouter::new_pinned(
        config(missing_home.path()),
        "missing".to_string(),
        RouterExternalAuthState::default(),
        upstream(),
    );
    assert!(matches!(
        missing.lease_for_turn_if_configured().await,
        Err(ProfileAuthRouterError::UnknownAccount)
    ));

    let disabled_home = tempdir().expect("temporary disabled home");
    let (mut disabled_account, _) = seed_account(disabled_home.path(), "disabled", "disabled");
    disabled_account.enabled = false;
    seed_registry(
        disabled_home.path(),
        vec![disabled_account],
        /*default*/ 0,
    );
    let disabled = SharedProfileAuthRouter::new_pinned(
        config(disabled_home.path()),
        "disabled".to_string(),
        RouterExternalAuthState::default(),
        upstream(),
    );
    assert!(matches!(
        disabled.lease_for_turn_if_configured().await,
        Err(ProfileAuthRouterError::DisabledAccount)
    ));

    let logged_out_home = tempdir().expect("temporary logged-out home");
    let logged_out = AccountMetadata::new(
        "logged-out".parse::<AccountAlias>().expect("alias"),
        AuthMode::ApiKey,
        Utc::now(),
    );
    seed_registry(logged_out_home.path(), vec![logged_out], /*default*/ 0);
    let logged_out = SharedProfileAuthRouter::new_pinned(
        config(logged_out_home.path()),
        "logged-out".to_string(),
        RouterExternalAuthState::default(),
        upstream(),
    );
    assert!(matches!(
        logged_out.lease_for_turn_if_configured().await,
        Err(ProfileAuthRouterError::NotAuthenticated)
    ));
}
