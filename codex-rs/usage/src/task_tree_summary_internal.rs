#[derive(Clone)]
pub(super) struct OperationRow {
    pub(super) id: String,
    pub(super) parent_operation_id: Option<String>,
    pub(super) retry_of_operation_id: Option<String>,
    pub(super) rework_of_operation_id: Option<String>,
    pub(super) kind: String,
    pub(super) agent_id: Option<String>,
    pub(super) started_at_ms: i64,
    pub(super) ended_at_ms: Option<i64>,
    pub(super) terminal_status: Option<String>,
    pub(super) error_category: Option<String>,
    pub(super) activity_state: String,
    pub(super) model_request_id: Option<String>,
}

#[derive(Default)]
pub(super) struct EffortAccumulator {
    pub(super) operations: u64,
    pub(super) model_requests: u64,
    pub(super) tokens: TokenAccumulator,
    pub(super) intervals: Vec<(i64, i64)>,
    pub(super) unknown_intervals: u64,
}

#[derive(Clone, Default)]
pub(super) struct TokenAccumulator {
    pub(super) measured_tokens: i64,
    pub(super) unknown_observations: u64,
    pub(super) has_gap: bool,
}

#[derive(Default)]
pub(super) struct OutcomeAccumulator {
    pub(super) count: u64,
    pub(super) intervals: Vec<(i64, i64)>,
    pub(super) unknown_intervals: u64,
}

pub(super) struct AgentMetadata {
    pub(super) role: String,
}
