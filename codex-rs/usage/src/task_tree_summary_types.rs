use crate::ThreadId;
use crate::UtcTimeRange;
use serde::Serialize;

pub const TASK_TREE_SUMMARY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTreeSummaryQuery {
    pub root_thread_id: ThreadId,
    pub include_descendants: bool,
    pub time_range: UtcTimeRange,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeSummary {
    pub schema_version: u32,
    pub kind: &'static str,
    pub database_schema_version: u64,
    pub root_thread_id: String,
    pub include_descendants: bool,
    pub time_range: TaskTreeTimeRange,
    pub counts: TaskTreeCounts,
    pub totals: TaskTreeEffort,
    pub agents: Vec<TaskTreeAgentSummary>,
    pub waits: TaskTreeWaitSummary,
    pub context: TaskTreeContextSummary,
    pub work: TaskTreeWorkSummary,
    pub formulas: TaskTreeFormulas,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeTimeRange {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeCounts {
    pub threads: u64,
    pub agents: u64,
    pub raw_operations: u64,
    pub deduplicated_operations: u64,
    pub model_requests: u64,
    pub raw_tool_operations: u64,
    pub deduplicated_tool_operations: u64,
    pub wrapper_tool_operations: u64,
    pub nested_tool_operations: u64,
    pub unlinked_wrapper_tool_operations: u64,
    pub unlinked_nested_tool_operations: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeTokenAggregate {
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeDuration {
    pub measured_ns: u64,
    pub exact_ns: Option<u64>,
    pub unknown_intervals: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeEffort {
    pub operations: u64,
    pub model_requests: u64,
    pub provider_total_tokens: TaskTreeTokenAggregate,
    pub wall_time: TaskTreeDuration,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeAgentSummary {
    pub agent_id: Option<String>,
    pub role: String,
    pub operations: u64,
    pub model_requests: u64,
    pub provider_total_tokens: TaskTreeTokenAggregate,
    pub wall_time: TaskTreeDuration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeOutcomeAggregate {
    pub count: u64,
    pub wall_time: TaskTreeDuration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeWaitSummary {
    pub completed: TaskTreeOutcomeAggregate,
    pub intentional_expiry: TaskTreeOutcomeAggregate,
    pub failed: TaskTreeOutcomeAggregate,
    pub cancelled: TaskTreeOutcomeAggregate,
    pub unknown: TaskTreeOutcomeAggregate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeContextSource {
    pub source: &'static str,
    pub estimated_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeContextSummary {
    pub estimator: &'static str,
    pub observed_requests: u64,
    pub unknown_requests: u64,
    pub sources: Vec<TaskTreeContextSource>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeWorkSummary {
    pub first_pass: TaskTreeEffort,
    pub post_integration_rework: TaskTreeEffort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTreeFormulas {
    pub operation_deduplication: &'static str,
    pub wall_time: &'static str,
    pub tokens: &'static str,
    pub context: &'static str,
    pub rework: &'static str,
    pub time_window: &'static str,
}
