use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// A measured token or millisecond quantity. `unknown` counts missing/incomplete
/// observations or intervals, not an estimate of the missing quantity.
pub struct OutcomeMeasurement {
    pub measured: u64,
    pub exact: Option<u64>,
    pub unknown: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeEffort {
    pub operations: u64,
    pub retry_operations: u64,
    pub rework_operations: u64,
    pub provider_total_tokens: OutcomeMeasurement,
    pub active_agent_ms: OutcomeMeasurement,
    pub elapsed_execution_ms: OutcomeMeasurement,
    pub recorded_wait_ms: OutcomeMeasurement,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeRow {
    pub outcome_id: String,
    pub workstream_id: Option<String>,
    pub effort: OutcomeEffort,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeReport {
    pub schema_version: u32,
    pub coverage: String,
    pub totals: OutcomeEffort,
    pub attributed: OutcomeEffort,
    pub unattributed: OutcomeEffort,
    pub unattributed_reasons: BTreeMap<String, u64>,
    pub rows: Vec<OutcomeRow>,
    pub total_rows: usize,
    pub next_cursor: Option<String>,
    pub basis: String,
}
