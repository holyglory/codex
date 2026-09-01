use codex_login::ManagedAccountPriorityMutation;
use codex_login::ManagedAccountSnapshot;
use codex_login::ManagedAccountSummary;
use codex_protocol::auth::AuthMode;
use pretty_assertions::assert_eq;

use super::MAX_TOOL_OUTPUT_BYTES;

const MAX_TOOL_SPEC_BYTES: usize = 4 * 1024;
use super::account_management_spec;
use super::account_output;
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
fn tool_contract_explains_priority_direction_and_security_boundary() {
    let encoded = serde_json::to_string(&account_management_spec()).expect("serialize spec");
    assert_eq!(encoded.len(), 1_454);
    assert!(encoded.len() <= MAX_TOOL_SPEC_BYTES);
    for required in [
        "set_priority",
        "set_all_priorities",
        "higher numbers drain first",
        "never returns email",
    ] {
        assert!(encoded.contains(required), "missing {required}");
    }
    let account = account("alpha", /*priority*/ 1000);
    assert_eq!(account_output(&account, Some("alpha")).priority, 1000);
}

#[test]
fn worst_case_outputs_fit_and_encoding_rejects_above_sixteen_kibibytes() {
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
                .map(|index| account(&format!("account{index}"), u32::MAX))
                .collect(),
        },
        /*offset*/ 0,
        /*limit*/ 10,
        /*routed_account*/ None,
    )
    .expect("maximal service usage page");
    usage.service_usage_bucket_fields = Some(service_usage::bucket_fields());
    for account in &mut usage.accounts {
        account.service_usage = Some(service_usage::maximal_service_usage());
    }
    let encoded_usage = encode_output(&usage).expect("10 accounts by 8 buckets stays bounded");
    assert!(encoded_usage.len() < MAX_TOOL_OUTPUT_BYTES);

    assert!(encode_output(&"x".repeat(MAX_TOOL_OUTPUT_BYTES)).is_err());
}
