use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageAccountingCapability {
    pub version: u32,
    #[serde(default)]
    pub supported_versions: Vec<u32>,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub storage: String,
    /// Accepts explicit read-only workContextBindings supplied on each request.
    #[serde(default)]
    pub supports_work_context_bindings: bool,
    #[serde(default)]
    pub supports_workstream_outcomes: bool,
    #[serde(default)]
    pub snapshot_retention_ms: u32,
}

/// Read one consistent usage-v2 snapshot. No legacy usage database is opened.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageSummaryParams {
    /// Omission selects version 1; unsupported versions are rejected.
    #[serde(default, alias = "schemaVersion")]
    pub contract_version: Option<u32>,
    #[serde(default, alias = "fromAt")]
    pub from_at_ms: Option<i64>,
    #[serde(default, alias = "toAt")]
    pub to_at_ms: Option<i64>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default, alias = "repositoryKey")]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Select outcome accounting only. Repository report/totals retain scope.
    #[serde(default)]
    pub workstream_id: Option<String>,
    /// Explicit authority declarations; at most 256, never persisted by this RPC.
    #[serde(default)]
    pub work_context_bindings: Option<Vec<LocalUsageWorkContextBinding>>,
    /// Reserved compatibility selector; non-null is rejected until captured.
    #[serde(default)]
    pub worktree_keys: Option<Vec<String>>,
    /// Compatibility flag, meaningful only alongside a worktree selector.
    #[serde(default)]
    pub include_unassigned: bool,
    #[serde(default)]
    pub include_review: bool,
    /// Zero/omission excludes detail; a requested page is limited to 50 rows.
    #[serde(default)]
    pub record_limit: u32,
    #[serde(default)]
    pub operation_limit: u32,
    #[serde(default)]
    pub records_after: Option<LocalUsageCursor>,
    #[serde(default)]
    pub operations_after: Option<LocalUsageCursor>,
    /// Required with a continuation, so paging never silently mixes snapshots.
    #[serde(default)]
    pub expected_snapshot: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageCursor {
    pub observed_at_ms: i64,
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageSnapshot {
    pub source_watermark: String,
    pub generated_at: u64,
    pub generation: u64,
    pub watermark: u64,
    pub journal_watermark: u64,
    pub watermark_at_ms: i64,
    pub updated_at_ms: i64,
    pub freshness: String,
    pub pending_entries: u64,
    pub pending_corrections: u64,
    pub refresh_id: Option<String>,
}

/// Only explicitly captured bindings may populate this versioned extension.
/// Repository/thread identity or operation completion never implies a binding.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct LocalUsageWorkContext {
    #[serde(alias = "schema_version")]
    pub schema_version: u32,
    pub id: String,
    #[serde(alias = "repository_id")]
    pub repository_id: Option<String>,
    #[serde(alias = "projectId", alias = "project_id", alias = "native_project_id")]
    pub native_project_id: Option<String>,
    #[serde(alias = "workstream_id")]
    pub workstream_id: Option<String>,
    #[serde(alias = "outcome_id")]
    pub outcome_id: Option<String>,
    #[serde(alias = "thread_id")]
    pub thread_id: Option<String>,
    #[serde(alias = "turn_id")]
    pub turn_id: Option<String>,
    #[serde(alias = "experiment_ref")]
    pub experiment_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct LocalUsageWorkContextBinding {
    #[serde(alias = "schema_version")]
    pub schema_version: u32,
    #[serde(alias = "operation_id")]
    pub operation_id: String,
    #[serde(alias = "captured_at_ms")]
    pub captured_at_ms: i64,
    #[serde(alias = "work_context")]
    pub work_context: LocalUsageWorkContext,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryIdentityParams {
    #[serde(default)]
    pub contract_version: Option<u32>,
    /// Private local input; never echoed in the response or error.
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryIdentityResponse {
    pub contract_version: u32,
    pub source: String,
    pub repository_id: Option<String>,
    pub format_version: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageOperationBinding {
    pub schema_version: u32,
    pub operation_id: String,
    pub work_context_id: String,
    pub captured_at_ms: i64,
}

/// Content-free summary with opt-in finite diagnostics. `report` preserves the
/// existing localUsage/summary envelope; `summary` is the native usage-v2 view.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageSummaryResponse {
    pub contract_version: u32,
    pub source: String,
    pub generated_at: u64,
    pub snapshot: LocalUsageSnapshot,
    pub report: Value,
    pub summary: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub review: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub observations: Option<Vec<Value>>,
    pub next_records_cursor: Option<LocalUsageCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub operations: Option<Vec<Value>>,
    pub next_operations_cursor: Option<LocalUsageCursor>,
    pub work_contexts: Vec<LocalUsageWorkContext>,
    pub operation_bindings: Vec<LocalUsageOperationBinding>,
}

#[cfg(test)]
mod tests {
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
}
