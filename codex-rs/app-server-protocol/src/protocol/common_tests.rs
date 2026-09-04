use super::*;
use anyhow::Result;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn client_response_payload_serializes_without_an_intermediate_json_value() -> Result<()> {
    let payload = ClientResponsePayload::ThreadArchive(v2::ThreadArchiveResponse {});
    assert_eq!(serde_json::to_string(&payload)?, "{}");
    let Some(ClientResponse::ThreadArchive {
        request_id,
        response: _,
    }) = payload.into_client_response(RequestId::Integer(7))
    else {
        panic!("expected thread/archive client response");
    };
    assert_eq!(request_id, RequestId::Integer(7));
    Ok(())
}

#[test]
fn interrupt_conversation_payload_stays_jsonrpc_only() -> Result<()> {
    let payload = ClientResponsePayload::InterruptConversation(v1::InterruptConversationResponse {
        abort_reason: TurnAbortReason::Interrupted,
    });
    assert_eq!(
        serde_json::to_value(&payload)?,
        json!({
            "abortReason": "interrupted",
        })
    );
    assert!(
        payload
            .into_client_response(RequestId::Integer(8))
            .is_none()
    );
    Ok(())
}

#[test]
fn multi_account_local_usage_methods_are_registered_as_experimental() {
    for method in [
        "accountProfile/list",
        "accountProfile/read",
        "accountProfile/activate",
        "accountProfile/update",
        "accountProfile/remove",
        "accountProfileLogin/start",
        "accountProfileLogin/cancel",
        "accountProfileRateLimit/read",
        "accountAutoSelection/read",
        "accountAutoSelection/write",
        "localUsage/summary",
        "localUsageThread/read",
        "localUsageRepository/list",
        "localUsageRepository/read",
        "localUsageRepository/update",
        "localUsageRepository/merge",
        "localUsageTool/list",
        "localUsageActivity/list",
        "localUsageEvent/list",
        "localUsageClassification/correct",
        "localUsageExport/create",
        "eventSubscription/create",
        "eventSubscription/list",
        "eventSubscription/cancel",
        "eventSubscription/trigger",
        "event/publish",
    ] {
        assert!(EXPERIMENTAL_CLIENT_METHODS.contains(&method), "{method}");
    }
}

#[test]
fn multi_account_local_usage_wire_names_are_camel_case() -> Result<()> {
    let request = ClientRequest::LocalUsageRepositoryList {
        request_id: RequestId::Integer(4),
        params: v2::LocalUsageRepositoryListParams {
            cursor: Some("cursor-1".to_string()),
            limit: Some(25),
        },
    };
    assert_eq!(
        serde_json::to_value(request)?,
        json!({
            "method": "localUsageRepository/list",
            "id": 4,
            "params": {"cursor": "cursor-1", "limit": 25}
        })
    );

    let notification = ServerNotification::AccountProfileActiveChanged(
        v2::AccountProfileActiveChangedNotification {
            account_id: "acct-2".to_string(),
            previous_account_id: Some("acct-1".to_string()),
            changed_at: 1_700_000_000,
            generation: 9,
        },
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&notification),
        Some("accountProfile/activeChanged")
    );
    assert_eq!(
        serde_json::to_value(notification)?,
        json!({
            "method": "accountProfile/activeChanged",
            "params": {
                "accountId": "acct-2",
                "previousAccountId": "acct-1",
                "changedAt": 1_700_000_000,
                "generation": 9
            }
        })
    );
    let usage_notification =
        ServerNotification::LocalUsageUpdated(v2::LocalUsageUpdatedNotification {
            generation: 10,
            updated_at: 1_700_000_001,
            thread_id: None,
            repository_key: Some("repo-key".to_string()),
        });
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&usage_notification),
        Some("localUsage/updated")
    );
    Ok(())
}
