//! The Coordinator review consumer's current strict wire shapes. Exercise
//! these against real IPC responses rather than a separately edited fixture.
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn assert_review(value: &Value) -> Result<()> {
    let _: ReviewUsage = serde_json::from_value(value.clone())
        .context("local usage review must satisfy the Coordinator consumer contract")?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewUsage {
    coverage: UsageCoverage,
    totals: UsageTotals,
    activities: Vec<UsageActivity>,
    time: UsageTime,
    tools: UsageTools,
    semantics: UsageSemantics,
    outcomes: OutcomeReport,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageCoverage {
    #[serde(default)]
    sources: Vec<Value>,
    snapshot: Option<Value>,
    loading: Option<Value>,
    state: String,
    has_gaps: bool,
    configured_collectors: u32,
    available_collectors: u32,
    contributing_collectors: u32,
    freshest_at_ms: Option<u64>,
    events: BTreeMap<String, u64>,
    token_observations: BTreeMap<String, u64>,
    unavailable_reasons: BTreeMap<String, u64>,
    database_schemas: Vec<u32>,
    taxonomy_versions: Vec<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageTotals {
    total_tokens: Option<u64>,
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    model_requests: u64,
    tool_calls: u64,
    operations: u64,
    #[serde(default)]
    cost: UsageCost,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageActivity {
    phase: String,
    activity: String,
    total_tokens: u64,
    share: Option<f64>,
    operations: Option<u64>,
    provenance: BTreeMap<String, u64>,
    #[serde(default)]
    cost: UsageCost,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MeasuredTime {
    measured_ms: u64,
    unknown_intervals: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PhaseTime {
    phase: String,
    measured_ms: u64,
    unknown_intervals: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageTime {
    request_to_delivery: MeasuredTime,
    execution_wall: MeasuredTime,
    summed_agent_active: MeasuredTime,
    phases: Vec<PhaseTime>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageTools {
    outcomes: Vec<ToolOutcome>,
    families: Vec<ToolFamily>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolOutcome {
    outcome: String,
    count: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolFamily {
    family: String,
    count: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageSemantics {
    tokens: String,
    time: String,
    coverage: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeMeasurement {
    measured: u64,
    exact: Option<u64>,
    unknown: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeEffort {
    #[serde(default)]
    activities: BTreeMap<String, OutcomeMeasurement>,
    operations: u64,
    retry_operations: u64,
    rework_operations: u64,
    provider_total_tokens: OutcomeMeasurement,
    active_agent_ms: OutcomeMeasurement,
    elapsed_execution_ms: OutcomeMeasurement,
    recorded_wait_ms: OutcomeMeasurement,
    #[serde(default)]
    cost: UsageCost,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeRow {
    kind: Option<String>,
    title: Option<String>,
    outcome_id: String,
    workstream_id: Option<String>,
    effort: OutcomeEffort,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeReport {
    #[serde(default)]
    kinds: BTreeMap<String, OutcomeMeasurement>,
    schema_version: u32,
    coverage: String,
    totals: OutcomeEffort,
    attributed: OutcomeEffort,
    unattributed: OutcomeEffort,
    unattributed_reasons: BTreeMap<String, u64>,
    rows: Vec<OutcomeRow>,
    total_rows: usize,
    next_cursor: Option<String>,
    basis: String,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageCost {
    status: String,
    basis: String,
    currency: String,
    #[serde(default)]
    processing_tier: String,
    estimated_usd: Option<String>,
    estimated_usd_micros: Option<u64>,
    input_usd_micros: Option<u64>,
    cached_input_usd_micros: Option<u64>,
    cache_write_usd_micros: Option<u64>,
    output_usd_micros: Option<u64>,
    reasoning_usd_micros: Option<u64>,
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_write_tokens: Option<u64>,
    uncached_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    #[serde(default)]
    unknown_requests: Option<u64>,
    #[serde(default)]
    unknown_tokens: u64,
    #[serde(default)]
    unknown_observations: Option<u64>,
    #[serde(default)]
    priced_observations: Option<u64>,
    rate_card_ref: Option<String>,
    #[serde(default)]
    rate_card_refs: Vec<String>,
    #[serde(default)]
    matched_rate_cards: Vec<Value>,
    #[serde(default)]
    model_requests: Option<u64>,
    #[serde(default)]
    priced_requests: Option<u64>,
    #[serde(default)]
    unavailable_reasons: BTreeMap<String, u64>,
}
