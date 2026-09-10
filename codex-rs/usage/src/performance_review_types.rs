use crate::RepositoryId;
use crate::ThreadId;
use crate::UtcTimeRange;
use serde::Serialize;

#[derive(Clone, Debug, Default)]
pub struct PerformanceReviewQuery {
    pub repository_id: Option<RepositoryId>,
    pub thread_id: Option<ThreadId>,
    pub time_range: Option<UtcTimeRange>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerformanceReviewPacket {
    pub schema_version: u32,
    pub kind: &'static str,
    pub window: ReviewWindow,
    pub tokens: Vec<ReviewTokens>,
    pub operations: Vec<ReviewCategory>,
    pub waits: Vec<ReviewCategory>,
    pub links: ReviewLinks,
    pub candidates: Vec<ReviewCandidate>,
    pub coverage: ReviewCoverage,
    pub work_bindings: ReviewWorkBindings,
    pub evidence: ReviewEvidence,
    pub interpretation: &'static str,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWindow {
    pub from_at_ms: Option<i64>,
    pub to_at_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewTokens {
    pub category: String,
    pub provenance: String,
    pub measured_tokens: u64,
    pub exact_tokens: Option<u64>,
    pub observations: u64,
    pub unknown_observations: u64,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCategory {
    pub category: String,
    pub count: u64,
    pub measured_interval_sum_ms: u64,
    pub unknown_intervals: u64,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewLinks {
    pub retry_operations: u64,
    pub rework_operations: u64,
    pub sample: Vec<ReviewLink>,
    pub omitted_operations: u64,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewLink {
    pub operation_id: String,
    pub started_at_ms: i64,
    pub retry_of_operation_id: Option<String>,
    pub rework_of_operation_id: Option<String>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCandidate {
    pub signal: &'static str,
    pub measured_interval_ms: Option<u64>,
    pub critical_path: &'static str,
    pub avoidability: &'static str,
    pub evidence: ReviewLink,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCoverage {
    pub raw_operations: u64,
    pub deduplicated_operations: u64,
    pub unknown_operation_intervals: u64,
    pub unclassified_operations: u64,
    pub operations_without_repository: u64,
    pub unlinked_tool_operations: u64,
    pub model_requests_without_provider_total: u64,
    pub raw_token_observations: u64,
    pub deduplicated_token_observations: u64,
    pub unknown_token_observations: u64,
    pub incomplete_token_observations: u64,
    pub conflicting_token_observations: u64,
    pub unattributed_coverage_events_in_window: u64,
    pub omitted_token_categories: u64,
    pub omitted_candidates: u64,
    pub coverage_events: Vec<ReviewCoverageCount>,
    pub collection_completeness: &'static str,
    pub critical_path: &'static str,
    pub repeated_input_comparison: &'static str,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCoverageCount {
    pub state: String,
    pub count: u64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ReviewEvidence {
    pub action: &'static str,
    pub details: [&'static str; 4],
    pub repository: Option<String>,
    pub thread_id: Option<String>,
    pub from_at_ms: Option<i64>,
    pub to_at_ms: Option<i64>,
    pub limit: u32,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWorkBindings {
    pub references: Vec<ReviewWorkBindingReference>,
    pub omitted_references: u64,
    pub bound_operations: u64,
    pub unknown_operations: u64,
    pub ambiguous_operations: u64,
    pub boundary_operations: u64,
    pub multi_repository_operations: u64,
    pub unknown_workstream_operations: u64,
    pub basis: &'static str,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewWorkBindingReference {
    pub event_id: String,
    pub thread_id: String,
    pub native_project_id: String,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
    pub experiment_ref: Option<String>,
    pub observed_at_ms: i64,
    pub provenance: String,
    pub bound_operations: u64,
}
