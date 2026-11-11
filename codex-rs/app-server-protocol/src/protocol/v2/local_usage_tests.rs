use super::*;
use crate::ClientRequest;
use crate::RequestId;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn local_usage_summary_preserves_wire_method_and_legacy_parameter_names() {
    let params: LocalUsageSummaryParams = serde_json::from_value(json!({
        "schemaVersion":1,"repositoryKey":"repo","fromAt":1_000,"toAt":2_000,
        "ignoredPrompt":"private content must not be forwarded"
    }))
    .unwrap();
    assert_eq!(params.contract_version, Some(1));
    assert_eq!(params.repository_id.as_deref(), Some("repo"));
    assert_eq!(params.from_at_ms, Some(1_000));
    assert_eq!(params.to_at_ms, Some(2_000));
    let value = serde_json::to_value(ClientRequest::LocalUsageSummary {
        request_id: RequestId::Integer(7),
        params,
    })
    .unwrap();
    assert_eq!(value["method"], "localUsage/summary");
    assert_eq!(value["params"]["contractVersion"], 1);
    assert!(value["params"].get("ignoredPrompt").is_none());
    assert!(value["params"].get("repositoryKey").is_none());
    let parsed: ClientRequest = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.method_name(), "localUsage/summary");
}
