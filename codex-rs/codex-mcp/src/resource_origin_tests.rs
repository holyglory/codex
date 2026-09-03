use super::*;
use codex_login::AuthManager;
use codex_login::CodexAuth;

#[test]
fn resource_origin_keeps_its_exact_live_binding_and_lease() {
    let binding_a = Arc::new(McpBinding::empty(Arc::new(
        crate::mcp::tests::test_mcp_config(std::env::temp_dir()),
    )));
    let binding_b = Arc::new(McpBinding::empty(Arc::new(
        crate::mcp::tests::test_mcp_config(std::env::temp_dir()),
    )));
    let lease_a = AuthManagerLease::legacy(AuthManager::from_auth_for_testing(
        CodexAuth::from_api_key("profile-a"),
    ));
    let lease_b = AuthManagerLease::legacy(AuthManager::from_auth_for_testing(
        CodexAuth::from_api_key("profile-b"),
    ));
    let mut origins = ResourceOrigins::default();
    origins.bind_call("call-a", Arc::clone(&binding_a), lease_a);
    origins.remember(
        "call-a",
        Some("turn-a"),
        CODEX_APPS_MCP_SERVER_NAME,
        "search",
        &serde_json::json!({}),
        Some("connector-a"),
        /*link_id*/ None,
        Some("ui://connector-a/widget"),
    );
    origins.bind_call("call-b", binding_b, lease_b);

    let (origin, binding, lease) = origins
        .bound_authority("call-a")
        .expect("known origin")
        .expect("live authority");

    assert_eq!(origin.call_id, "call-a");
    assert!(Arc::ptr_eq(&binding, &binding_a));
    assert_eq!(
        lease
            .auth_manager()
            .auth_cached()
            .expect("profile A auth")
            .get_token()
            .expect("profile A token"),
        "profile-a"
    );
}

#[test]
fn restored_origin_preserves_profile_identity_without_reusing_a_live_binding() {
    let account_id = "acct_00000000000000000000000000000001";
    let checkpoint = McpResourceOriginCheckpoint {
        origins: vec![McpResourceOrigin {
            call_id: "call-a".to_string(),
            turn_id: Some("turn-a".to_string()),
            account_id: Some(account_id.to_string()),
            tool: "search".to_string(),
            connector_id: "connector-a".to_string(),
            link_id: None,
            uri: "ui://connector-a/widget".to_string(),
            ambiguous_account: false,
        }],
        turns: vec!["turn-a".to_string()],
        current_turn_id: Some("turn-a".to_string()),
    };
    let mut origins = ResourceOrigins::default();
    origins.restore_checkpoint(&checkpoint);

    assert_eq!(
        origins.account_id("call-a").expect("known origin"),
        Some(account_id.to_string())
    );
    assert!(
        origins
            .bound_authority("call-a")
            .expect("known origin")
            .is_none()
    );
}
