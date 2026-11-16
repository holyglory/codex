//! Bounded context, cost, and interval metrics used by performance reports.
use std::collections::BTreeMap;

use duckdb::Connection;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::Result;
use crate::review_storage::query_rows;

#[path = "report_metrics_sql.rs"]
mod sql;

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct IntervalMetrics {
    pub active_ms: u64,
    pub elapsed_ms: u64,
    pub wait_ms: u64,
    pub active_unknown: u64,
    pub elapsed_unknown: u64,
    pub wait_unknown: u64,
    pub unknown_reasons: BTreeMap<String, u64>,
    pub first_started_at_ms: Option<i64>,
    pub last_ended_at_ms: Option<i64>,
    pub operations: u64,
    execution_observations: u64,
    wait_observations: u64,
}

impl IntervalMetrics {
    pub(crate) fn value(&self) -> Value {
        json!({
            "activeMs": measurement(self.active_ms, self.active_unknown, self.execution_observations),
            "elapsedMs": measurement(self.elapsed_ms, self.elapsed_unknown, self.execution_observations),
            "wallTimeMs": measurement(self.elapsed_ms, self.elapsed_unknown, self.execution_observations),
            "waitMs": measurement(self.wait_ms, self.wait_unknown, self.wait_observations),
            "unknownReasons": self.unknown_reasons,
            "coverage": if self.operations == 0 { "unobserved" } else if self.active_unknown != 0 || self.elapsed_unknown != 0 || self.wait_unknown != 0 { "partial" } else { "recorded" },
        })
    }

    pub(crate) fn lifecycle_value(&self) -> Value {
        json!({
            "firstStartedAtMs": self.first_started_at_ms,
            "lastEndedAtMs": self.last_ended_at_ms,
            "wallTimeMs": measurement(self.elapsed_ms, self.elapsed_unknown, self.execution_observations),
            "unknownReasons": self.unknown_reasons,
            "coverage": if self.operations == 0 { "unobserved" } else if self.unknown_reasons.is_empty() { "recorded" } else { "partial" },
        })
    }
}

/// Aggregate independent operation sets without materializing their intervals.
pub(crate) fn grouped_interval_metrics(
    connection: &Connection,
    operations: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<BTreeMap<String, IntervalMetrics>> {
    #[derive(Deserialize)]
    struct Group {
        timing_group: String,
        #[serde(flatten)]
        metrics: IntervalMetrics,
    }
    Ok(query_rows::<Group>(
        connection,
        &sql::grouped_interval_query(operations, start_ms, end_ms),
        /*max_rows*/ 16_643,
    )?
    .into_iter()
    .map(|row| (row.timing_group, row.metrics))
    .collect())
}

/// An observed zero is exact; an empty selection remains unobserved.
fn measurement(measured: u64, unknown: u64, observations: u64) -> Value {
    json!({
        "measured": measured,
        "exact": (unknown == 0 && observations > 0).then_some(measured),
        "unknown": unknown,
    })
}

pub(crate) fn context_and_cost_metrics(
    connection: &Connection,
    contributions: &str,
) -> Result<(Value, Value)> {
    let totals = query_rows::<Value>(
        connection,
        &sql::context_and_cost_query(contributions),
        /*max_rows*/ 1,
    )?
    .remove(0);
    let context: ContextTotals = serde_json::from_value(totals.clone())?;
    let cost: CostTotals = serde_json::from_value(totals)?;
    Ok((context.value(), cost.value()))
}

#[derive(Deserialize)]
struct ContextTotals {
    observations: u64,
    prompt_tokens: u64,
    context_tokens: u64,
    context_window_tokens: u64,
    cache_tokens: u64,
    compaction_count: u64,
    prompt_unknown: u64,
    context_unknown: u64,
    context_window_unknown: u64,
    cache_unknown: u64,
    compaction_unknown: u64,
    window_classes: Vec<Value>,
    window_classes_omitted: u64,
    window_class_observations_omitted: u64,
    window_unknown: u64,
}

impl ContextTotals {
    fn value(&self) -> Value {
        json!({
            "coverage": if self.observations == 0 { "unobserved" } else if self.prompt_unknown != 0 || self.context_unknown != 0 || self.cache_unknown != 0 || self.compaction_unknown != 0 || self.context_window_unknown != 0 || self.window_unknown != 0 { "partial" } else { "recorded" },
            "observations": self.observations,
            "promptTokens": measurement(self.prompt_tokens, self.prompt_unknown, self.observations),
            "contextTokens": measurement(self.context_tokens, self.context_unknown, self.observations),
            "contextWindowTokens": measurement(self.context_window_tokens, self.context_window_unknown, self.observations),
            "cacheTokens": measurement(self.cache_tokens, self.cache_unknown, self.observations),
            "compactionCount": measurement(self.compaction_count, self.compaction_unknown, self.observations),
            "windowClasses": self.window_classes,
            "windowClassesOmitted": self.window_classes_omitted,
            "windowClassObservationsOmitted": self.window_class_observations_omitted,
            "unknownWindowClass": self.window_unknown,
        })
    }
}

#[derive(Deserialize)]
struct CostTotals {
    observations: u64,
    estimated: u64,
    actual: u64,
    estimated_unknown: u64,
    actual_unknown: u64,
    provenance: Vec<Value>,
    provenance_omitted: u64,
    provenance_observations_omitted: u64,
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: u64,
    output_tokens: i64,
    reasoning_tokens: i64,
    provider_total_tokens: u64,
    priced_observations: u64,
    rate_card_refs: Vec<String>,
    rate_card_refs_omitted: u64,
    rate_card_observations_omitted: u64,
}

impl CostTotals {
    fn value(&self) -> Value {
        let has_estimate = self.priced_observations > 0;
        // A provider total does not supply the component prices required by
        // consumers for complete cost coverage. Preserve the known estimate,
        // including zero, without inventing an input/output price breakdown.
        let status = if has_estimate {
            "partial"
        } else {
            "unavailable"
        };
        let mut unavailable_reasons = BTreeMap::new();
        if self.observations == 0 {
            unavailable_reasons.insert("no_token_observations", 1);
        }
        if self.estimated_unknown > 0 {
            unavailable_reasons.insert("rate_card_missing", self.estimated_unknown);
        }
        if has_estimate {
            unavailable_reasons.insert(
                "component_cost_breakdown_unavailable",
                self.priced_observations,
            );
        }
        let estimated_usd = has_estimate.then(|| {
            format!(
                "{}.{:06}",
                self.estimated / 1_000_000,
                self.estimated % 1_000_000
            )
        });
        json!({
            "status": status,
            "basis": "api_equivalent",
            "currency": "USD",
            "processingTier": "standard",
            "estimatedUsd": estimated_usd,
            "estimatedUsdMicros": has_estimate.then_some(self.estimated),
            "inputUsdMicros": Value::Null,
            "cachedInputUsdMicros": Value::Null,
            "cacheWriteUsdMicros": Value::Null,
            "outputUsdMicros": Value::Null,
            "inputTokens": self.input_tokens,
            "cachedInputTokens": self.cached_input_tokens,
            "cacheWriteTokens": self.cache_write_tokens,
            "uncachedInputTokens": self.input_tokens.saturating_sub(self.cached_input_tokens),
            "outputTokens": self.output_tokens,
            "reasoningTokens": self.reasoning_tokens,
            "providerTotalTokens": self.provider_total_tokens,
            "pricedObservations": self.priced_observations,
            "unknownObservations": self.estimated_unknown,
            "unknownRequests": Value::Null,
            "unknownTokens": 0,
            "rateCardRefs": self.rate_card_refs,
            "rateCardRefsOmitted": self.rate_card_refs_omitted,
            "rateCardObservationsOmitted": self.rate_card_observations_omitted,
            "unavailableReasons": unavailable_reasons,
            "pricingBasis": "api_equivalent",
            "estimatedApiEquivalentCostMicros": measurement(self.estimated, self.estimated_unknown, self.observations),
            "actualBillingMicros": measurement(self.actual, self.actual_unknown, self.observations),
            "provenance": self.provenance,
            "provenanceOmitted": self.provenance_omitted,
            "provenanceObservationsOmitted": self.provenance_observations_omitted,
        })
    }
}
