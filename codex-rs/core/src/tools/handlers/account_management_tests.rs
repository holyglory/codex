use codex_login::ManagedAccountPriorityMutation;
use codex_login::ManagedAccountSnapshot;
use codex_login::ManagedAccountSummary;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;

use super::MAX_TOOL_OUTPUT_BYTES;

const MAX_TOOL_SPEC_BYTES: usize = 4 * 1024;
use super::account_management_spec;
use super::account_page;
use super::all_priorities_output;
use super::encode_output;
use super::service_usage;

fn account(alias: &str, priority: u32) -> ManagedAccountSummary {
    ManagedAccountSummary {
        account_id: format!("internal-{alias}"),
        alias: alias.to_string(),
        auth_mode: AuthMode::Chatgpt,
        enabled: true,
        credit_usage_enabled: false,
        authenticated: true,
        priority,
        is_default: alias == "alpha",
    }
}

#[test]
fn account_output_is_bounded_and_omits_internal_identity() {
    let page = account_page(
        ManagedAccountSnapshot {
            generation: 7,
            auto_selection_enabled: true,
            accounts: vec![
                account("alpha", /*priority*/ 1000),
                account("beta", /*priority*/ 900),
            ],
        },
        /*offset*/ 0,
        /*limit*/ 1,
        Some("alpha".to_string()),
    )
    .expect("page");
    let encoded = serde_json::to_value(page).expect("serialize page");
    assert_eq!(
        encoded,
        serde_json::json!({
            "generation": 7,
            "autoSelectionEnabled": true,
            "priorityOrder": "higherFirst",
            "routedAccount": "alpha",
            "accounts": [{
                "alias": "alpha",
                "authMode": "chatgpt",
                "enabled": true,
                "creditUsageEnabled": false,
                "authenticated": true,
                "priority": 1000,
                "isDefault": true,
                "isCurrentTurn": true
            }],
            "nextOffset": 1
        })
    );
    assert!(!encoded.to_string().contains("internal-alpha"));
}

#[test]
fn all_priority_output_caps_account_aliases() {
    let accounts = (0..30)
        .map(|index| account(&format!("account{index}"), /*priority*/ 1000))
        .collect();
    let output = all_priorities_output(
        ManagedAccountPriorityMutation {
            changed_count: 30,
            snapshot: ManagedAccountSnapshot {
                generation: 2,
                auto_selection_enabled: true,
                accounts,
            },
        },
        /*priority*/ 1000,
    );
    assert_eq!(output.accounts.len(), 25);
    assert!(output.accounts_truncated);
}

#[test]
fn worst_case_outputs_fit_and_encoding_rejects_above_sixteen_kibibytes() {
    assert!(
        serde_json::to_vec(&account_management_spec())
            .expect("tool schema")
            .len()
            <= MAX_TOOL_SPEC_BYTES
    );
    let maximal_accounts = (0..25)
        .map(|index| {
            let suffix = format!("{index:02}");
            account(&format!("a{}{}", "x".repeat(61), suffix), u32::MAX)
        })
        .collect::<Vec<_>>();
    let list = account_page(
        ManagedAccountSnapshot {
            generation: u64::MAX,
            auto_selection_enabled: true,
            accounts: maximal_accounts,
        },
        /*offset*/ 0,
        /*limit*/ 25,
        /*routed_account*/ None,
    )
    .expect("maximal account list");
    let encoded_list = encode_output(&list).expect("maximal account list stays bounded");
    assert!(encoded_list.len() < MAX_TOOL_OUTPUT_BYTES);

    let mut usage = account_page(
        ManagedAccountSnapshot {
            generation: u64::MAX,
            auto_selection_enabled: true,
            accounts: (0..10)
                .map(|index| account(&format!("a{}{index:02}", "x".repeat(61)), u32::MAX))
                .collect(),
        },
        /*offset*/ 0,
        super::validated_limit(Some(10), /*refresh_service_usage*/ true).unwrap(),
        /*routed_account*/ None,
    )
    .expect("maximal service usage page");
    usage.service_usage_bucket_fields = Some(service_usage::SERVICE_USAGE_BUCKET_FIELDS);
    for account in &mut usage.accounts {
        account.service_usage = Some(service_usage::ServiceUsageOutput {
            state: "observed",
            reason: None,
            observed_at: Some(i64::MAX),
            next_reset_at: Some(i64::MAX),
            next_reset_at_utc: Some("9999-12-31 23:59:59 UTC".into()),
            next_reset_scope: Some("codex.individual"),
            buckets: (0..8)
                .map(|index| {
                    service_usage::ServiceUsageBucket(
                        format!("b{index:0>31}"),
                        Some(100.0),
                        Some(i64::MAX),
                        Some(100.0),
                        Some(i64::MAX),
                        Some(true),
                        Some(true),
                        Some(100),
                        Some(i64::MAX),
                        Some(true),
                        Some("workspaceMemberUsageLimitReached"),
                    )
                })
                .collect(),
        });
    }
    assert_eq!(usage.next_offset, Some(usage.accounts.len() as u32));
    let encoded_usage =
        encode_output(&usage).expect("refreshed page stays bounded with reset dates");
    assert!(encoded_usage.len() < MAX_TOOL_OUTPUT_BYTES);

    assert!(encode_output(&"x".repeat(MAX_TOOL_OUTPUT_BYTES)).is_err());
}
