use super::AppServerCapabilities;
use codex_app_server_protocol::LocalUsageAccountingCapability;
use codex_app_server_protocol::MultiAccountCapability;
use pretty_assertions::assert_eq;

#[test]
fn advertised_capabilities_keep_supported_versions() {
    let capabilities = AppServerCapabilities::from_advertised(
        Some(MultiAccountCapability {
            version: 2,
            supports_managed_login: true,
            supports_auto_selection: true,
            supports_credit_usage: true,
        }),
        Some(LocalUsageAccountingCapability { version: 2 }),
        None,
    );

    assert_eq!(
        capabilities,
        AppServerCapabilities {
            multi_account: Some(MultiAccountCapability {
                version: 2,
                supports_managed_login: true,
                supports_auto_selection: true,
                supports_credit_usage: true,
            }),
            local_usage_accounting: Some(LocalUsageAccountingCapability { version: 2 }),
            event_subscriptions: None,
        }
    );
    assert!(capabilities.supports_multi_account());
    assert!(capabilities.supports_local_usage_accounting());
}

#[test]
fn absent_and_unknown_versions_are_unavailable() {
    let capabilities = AppServerCapabilities::from_advertised(
        Some(MultiAccountCapability {
            version: 1,
            supports_managed_login: true,
            supports_auto_selection: true,
            supports_credit_usage: true,
        }),
        Some(LocalUsageAccountingCapability { version: 99 }),
        None,
    );

    assert_eq!(capabilities, AppServerCapabilities::default());
    assert!(!capabilities.supports_multi_account());
    assert!(!capabilities.supports_local_usage_accounting());
}

#[test]
fn malformed_or_missing_initialize_fields_are_unavailable() {
    assert_eq!(
        AppServerCapabilities::from_initialize_result(&serde_json::json!({
            "multiAccount": {"version": "unknown"},
            "localUsageAccounting": null,
        })),
        AppServerCapabilities::default()
    );
    assert_eq!(
        AppServerCapabilities::from_initialize_result(&serde_json::json!({
            "userAgent": "codex_cli_rs/0.0.0",
            "codexHome": "/server/.codex",
        })),
        AppServerCapabilities::default()
    );
}

#[test]
fn in_process_contract_advertises_current_versions() {
    assert_eq!(
        AppServerCapabilities::advertised_v1(),
        AppServerCapabilities::from_advertised(
            Some(MultiAccountCapability {
                version: 2,
                supports_managed_login: true,
                supports_auto_selection: true,
                supports_credit_usage: true,
            }),
            Some(LocalUsageAccountingCapability { version: 2 }),
            None,
        )
    );
}
