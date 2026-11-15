//! Bounded context, cost, and interval metrics used by performance reports.
use std::collections::BTreeMap;

use duckdb::Connection;
use serde_json::Value;
use serde_json::json;

use crate::Result;
use crate::UsageContextSize;
use crate::UsageCostProvenance;
use crate::review_storage::query_rows;

const MAX_ROWS: usize = 16_384;

#[derive(Clone, Debug, Default)]
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
}

impl IntervalMetrics {
    pub(crate) fn value(&self) -> Value {
        json!({
            "activeMs": measurement(self.active_ms, self.active_unknown),
            "elapsedMs": measurement(self.elapsed_ms, self.elapsed_unknown),
            "wallTimeMs": measurement(self.elapsed_ms, self.elapsed_unknown),
            "waitMs": measurement(self.wait_ms, self.wait_unknown),
            "unknownReasons": self.unknown_reasons,
            "coverage": if self.operations == 0 { "unobserved" } else if self.active_unknown != 0 || self.elapsed_unknown != 0 || self.wait_unknown != 0 { "partial" } else { "recorded" },
        })
    }

    pub(crate) fn lifecycle_value(&self) -> Value {
        json!({
            "firstStartedAtMs": self.first_started_at_ms,
            "lastEndedAtMs": self.last_ended_at_ms,
            "wallTimeMs": measurement(self.elapsed_ms, self.elapsed_unknown),
            "unknownReasons": self.unknown_reasons,
            "coverage": if self.operations == 0 { "unobserved" } else if self.unknown_reasons.is_empty() { "recorded" } else { "partial" },
        })
    }
}

/// Compute union-safe elapsed, active-agent, and wait intervals from operation
/// identities. Summed operation durations remain diagnostic only.
pub(crate) fn interval_metrics(
    connection: &Connection,
    operations: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<IntervalMetrics> {
    let rows = query_rows::<Value>(
        connection,
        &format!(
            "SELECT kind, started_at_ms, ended_at_ms, agent_id, thread_id FROM ({operations})"
        ),
        MAX_ROWS,
    )?;
    let mut metrics = IntervalMetrics {
        operations: rows.len() as u64,
        ..Default::default()
    };
    let mut elapsed = Vec::new();
    let mut waits = Vec::new();
    let mut active = BTreeMap::<String, Vec<(i64, i64)>>::new();
    for row in rows {
        let kind = row["kind"].as_str().unwrap_or("unknown");
        let Some(start) = row["started_at_ms"].as_i64() else {
            unknown(&mut metrics, kind, "missing_start");
            continue;
        };
        let Some(end) = row["ended_at_ms"].as_i64() else {
            unknown(&mut metrics, kind, "missing_end");
            continue;
        };
        if end < start {
            unknown(&mut metrics, kind, "invalid_interval");
            continue;
        }
        metrics.first_started_at_ms = Some(
            metrics
                .first_started_at_ms
                .map_or(start, |value| value.min(start)),
        );
        metrics.last_ended_at_ms =
            Some(metrics.last_ended_at_ms.map_or(end, |value| value.max(end)));
        let start = start.max(start_ms);
        let end = end.min(end_ms);
        if start >= end {
            continue;
        }
        if kind == "wait" {
            waits.push((start, end));
            continue;
        }
        elapsed.push((start, end));
        let Some(owner) = row["agent_id"]
            .as_str()
            .or_else(|| row["thread_id"].as_str())
        else {
            unknown(&mut metrics, kind, "missing_active_owner");
            continue;
        };
        active
            .entry(owner.to_owned())
            .or_default()
            .push((start, end));
    }
    metrics.elapsed_ms = union_ms(&mut elapsed);
    metrics.wait_ms = union_ms(&mut waits);
    metrics.active_ms = active.values_mut().map(union_ms).sum();
    Ok(metrics)
}

fn unknown(metrics: &mut IntervalMetrics, kind: &str, reason: &str) {
    let key = format!("{kind}:{reason}");
    *metrics.unknown_reasons.entry(key).or_default() += 1;
    match kind {
        "wait" => metrics.wait_unknown += 1,
        _ => {
            metrics.elapsed_unknown += 1;
            metrics.active_unknown += 1;
        }
    }
}

fn union_ms(intervals: &mut Vec<(i64, i64)>) -> u64 {
    intervals.sort_unstable();
    let mut total = 0_u64;
    let mut current = None;
    for (start, end) in intervals.drain(..) {
        if let Some((current_start, current_end)) = current {
            if start <= current_end {
                current = Some((current_start, current_end.max(end)));
            } else {
                total = total.saturating_add((current_end - current_start) as u64);
                current = Some((start, end));
            }
        } else {
            current = Some((start, end));
        }
    }
    if let Some((start, end)) = current {
        total.saturating_add((end - start) as u64)
    } else {
        total
    }
}

fn measurement(measured: u64, unknown: u64) -> Value {
    json!({
        "measured": measured,
        "exact": (unknown == 0 && measured != 0).then_some(measured),
        "unknown": unknown,
    })
}

pub(crate) fn context_and_cost_metrics(
    connection: &Connection,
    contributions: &str,
) -> Result<(Value, Value)> {
    let rows = query_rows::<Value>(
        connection,
        &format!(
            "SELECT context_size, cost_provenance, actual_billing_micros, \
             COALESCE(observation_count, 1)::UBIGINT observation_count \
             FROM ({contributions}) \
             WHERE ({})",
            crate::review_aggregate::TOKEN_PREDICATE
        ),
        MAX_ROWS,
    )?;
    let mut context = ContextTotals::default();
    let mut cost = CostTotals::default();
    for row in rows {
        let count = row["observation_count"].as_u64().unwrap_or(1);
        context.observations = context.observations.saturating_add(count);
        cost.observations = cost.observations.saturating_add(count);
        // Retained rollups may carry a representative JSON value for many source
        // observations. Do not multiply that representative into a false exact total.
        let context_size = (count == 1)
            .then(|| parse_json::<UsageContextSize>(&row["context_size"]))
            .flatten();
        context.add(context_size.as_ref(), count);
        let provenance = (count == 1)
            .then(|| parse_json::<UsageCostProvenance>(&row["cost_provenance"]))
            .flatten();
        cost.add(
            provenance.as_ref(),
            row["actual_billing_micros"].as_u64(),
            count,
        );
    }
    Ok((context.value(), cost.value()))
}

#[derive(Default)]
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
    window_classes: BTreeMap<String, u64>,
    window_unknown: u64,
}

impl ContextTotals {
    fn add(&mut self, value: Option<&UsageContextSize>, count: u64) {
        let Some(value) = value else {
            self.prompt_unknown = self.prompt_unknown.saturating_add(count);
            self.context_unknown = self.context_unknown.saturating_add(count);
            self.context_window_unknown = self.context_window_unknown.saturating_add(count);
            self.cache_unknown = self.cache_unknown.saturating_add(count);
            self.compaction_unknown = self.compaction_unknown.saturating_add(count);
            self.window_unknown = self.window_unknown.saturating_add(count);
            return;
        };
        self.prompt_tokens = self.prompt_tokens.saturating_add(
            value
                .prompt_tokens
                .unwrap_or_default()
                .saturating_mul(count),
        );
        self.context_tokens = self.context_tokens.saturating_add(
            value
                .context_tokens
                .unwrap_or_default()
                .saturating_mul(count),
        );
        self.context_window_tokens = self.context_window_tokens.saturating_add(
            value
                .context_window_tokens
                .unwrap_or_default()
                .saturating_mul(count),
        );
        self.cache_tokens = self
            .cache_tokens
            .saturating_add(value.cache_tokens.unwrap_or_default().saturating_mul(count));
        self.compaction_count = self.compaction_count.saturating_add(
            value
                .compaction_count
                .unwrap_or_default()
                .saturating_mul(count),
        );
        self.prompt_unknown += u64::from(value.prompt_tokens.is_none()).saturating_mul(count);
        self.context_unknown += u64::from(value.context_tokens.is_none()).saturating_mul(count);
        self.context_window_unknown = self
            .context_window_unknown
            .saturating_add(u64::from(value.context_window_tokens.is_none()).saturating_mul(count));
        self.cache_unknown += u64::from(value.cache_tokens.is_none()).saturating_mul(count);
        self.compaction_unknown +=
            u64::from(value.compaction_count.is_none()).saturating_mul(count);
        if let Some(class) = &value.window_class {
            *self.window_classes.entry(class.clone()).or_default() += count;
        } else {
            self.window_unknown = self.window_unknown.saturating_add(count);
        }
    }

    fn value(&self) -> Value {
        json!({
            "coverage": if self.observations == 0 { "unobserved" } else if self.prompt_unknown != 0 || self.context_unknown != 0 || self.cache_unknown != 0 || self.compaction_unknown != 0 || self.context_window_unknown != 0 || self.window_unknown != 0 { "partial" } else { "recorded" },
            "observations": self.observations,
            "promptTokens": measurement(self.prompt_tokens, self.prompt_unknown),
            "contextTokens": measurement(self.context_tokens, self.context_unknown),
            "contextWindowTokens": measurement(self.context_window_tokens, self.context_window_unknown),
            "cacheTokens": measurement(self.cache_tokens, self.cache_unknown),
            "compactionCount": measurement(self.compaction_count, self.compaction_unknown),
            "windowClasses": self.window_classes.iter().map(|(class, observations)| json!({"class": class, "observations": observations})).collect::<Vec<_>>(),
            "unknownWindowClass": self.window_unknown,
        })
    }
}

#[derive(Default)]
struct CostTotals {
    observations: u64,
    estimated: u64,
    actual: u64,
    estimated_unknown: u64,
    actual_unknown: u64,
    provenance: BTreeMap<String, (UsageCostProvenance, u64)>,
}

impl CostTotals {
    fn add(&mut self, value: Option<&UsageCostProvenance>, actual: Option<u64>, count: u64) {
        let Some(value) = value else {
            self.estimated_unknown = self.estimated_unknown.saturating_add(count);
            if let Some(amount) = actual {
                self.actual = self.actual.saturating_add(amount);
            } else {
                self.actual_unknown = self.actual_unknown.saturating_add(count);
            }
            return;
        };
        if let Some(amount) = value.estimated_api_equivalent_cost_micros {
            self.estimated = self.estimated.saturating_add(amount.saturating_mul(count));
        } else {
            self.estimated_unknown = self.estimated_unknown.saturating_add(count);
        }
        if let Some(amount) = actual {
            self.actual = self.actual.saturating_add(amount);
        } else {
            self.actual_unknown = self.actual_unknown.saturating_add(count);
        }
        let key = serde_json::to_string(value).unwrap_or_default();
        let entry = self
            .provenance
            .entry(key)
            .or_insert_with(|| (value.clone(), 0));
        entry.1 = entry.1.saturating_add(count);
    }

    fn value(&self) -> Value {
        json!({
            "coverage": if self.observations == 0 { "unobserved" } else if self.estimated_unknown != 0 || self.actual_unknown != 0 { "partial" } else { "recorded" },
            "pricingBasis": "api_equivalent",
            "estimatedApiEquivalentCostMicros": measurement(self.estimated, self.estimated_unknown),
            "actualBillingMicros": measurement(self.actual, self.actual_unknown),
            "provenance": self.provenance.values().map(|(value, observations)| json!({"model":value.model,"provider":value.provider,"serviceTier":value.service_tier,"contextClass":value.context_class,"region":value.region,"rateCard":value.rate_card,"rateCardVersion":value.rate_card_version,"pricingBasis":value.pricing_basis,"observations":observations,"estimatedApiEquivalentCostMicros":value.estimated_api_equivalent_cost_micros})).collect::<Vec<_>>(),
        })
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(value: &Value) -> Option<T> {
    let value = value.as_str()?;
    serde_json::from_str(value).ok()
}
