use super::*;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::CodexAuth;
use codex_login::ProfileAuthStorage;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;
use serde_json::json;

/// The public chat fixture does not expose RegularTask's boundary after a sampling loop returns.
/// Exercise its repeated auth-selection boundary using real leases and retained turn contexts.
#[tokio::test]
#[serial_test::serial(codex_auth_env)]
async fn rejected_profiles_survive_same_turn_reentry_and_authentication_forks() -> anyhow::Result<()>
{
    let home = tempfile::tempdir()?;
    let mut accounts = Vec::new();
    for alias in ["a", "b"] {
        let account = AccountMetadata::new(alias.parse()?, AuthMode::Chatgpt, chrono::Utc::now());
        let auth: AuthDotJson = serde_json::from_value(json!({
            "auth_mode": "chatgpt", "tokens": {"id_token":"e30.e30.signature",
                "access_token":format!("{alias}-access"), "refresh_token":"refresh",
                "account_id":format!("{alias}-workspace")},
            "last_refresh":chrono::Utc::now()
        }))?;
        ProfileAuthStorage::new(
            home.path(),
            account.id.clone(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )?
        .save(&auth)?;
        accounts.push(account);
    }
    let mut registry = AccountRegistry {
        accounts: accounts.clone(),
        default_account_id: Some(accounts[0].id.clone()),
        ..AccountRegistry::default()
    };
    registry.auto_selection.enabled = true;
    RegistryStore::new(home.path()).create(&registry)?;
    let (session, turn, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("upstream"),
            Vec::new(),
            home.path(),
            |config| config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File,
        )
        .await;
    let router = SharedProfileAuthRouter::new_with_external_auth(
        turn.config.auth_config(),
        RouterExternalAuthState::default(),
        Arc::clone(&session.services.auth_manager),
    );
    let snapshot: RateLimitSnapshot = serde_json::from_value(json!({"primary":{
        "used_percent":10, "window_minutes":300, "resets_at":chrono::Utc::now().timestamp()+3600
    }}))?;
    let observed_at = chrono::Utc::now().timestamp() - 1;
    for account in &accounts {
        router.record_rate_limits(account.id.clone(), observed_at, vec![snapshot.clone()])?;
    }
    let lease = router
        .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
        .await?
        .expect("initial managed lease");
    assert_eq!(lease.account_id(), &accounts[0].id);
    let initial = turn.with_auth_manager(Arc::clone(lease.auth_manager()));
    initial.install_turn_auth(
        &session,
        TurnAuth {
            lease: Some(AuthManagerLease::profile(lease)),
            error: None,
        },
    );
    session
        .services
        .thread_extension_data
        .insert(router.clone());
    // A headerless response is emitted as an empty snapshot. Use a newer timestamp
    // than the probe so this also catches overwrites outside the same-second conflict.
    session
        .record_rate_limits_for_turn(&initial, serde_json::from_value(json!({}))?)
        .await;
    assert_eq!(
        router
            .lease_for_turn_with_external_auth(RouterExternalAuthState::default())
            .await?
            .expect("headerless response preserves observed capacity")
            .account_id(),
        &accounts[0].id
    );
    let backup = session
        .failover_turn_auth(&initial)
        .await
        .expect("eligible backup");
    assert_eq!(backup.local_profile_id(), Some(accounts[1].id.to_string()));
    // Both profiles still have available cached capacity. Only this logical turn's rejection
    // history prevents returning to A when B rejects after the sampling loop is entered again.
    assert!(session.failover_turn_auth(&backup).await.is_none());
    assert!(session.failover_turn_auth(&initial).await.is_none());
    Ok(())
}
