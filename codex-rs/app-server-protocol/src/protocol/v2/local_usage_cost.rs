use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// A versioned standard API-equivalent estimate, not an invoice or subscription charge.
/// Missing observations and unpriced models remain explicit in status and counts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportCost {
    pub basis: String,
    pub currency: String,
    pub status: String,
    pub processing_tier: String,
    pub estimated_usd_micros: Option<u64>,
    pub input_usd_micros: Option<u64>,
    pub cached_input_usd_micros: Option<u64>,
    pub cache_write_usd_micros: Option<u64>,
    pub output_usd_micros: Option<u64>,
    pub input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub provider_total_tokens: u64,
    pub priced_observations: u64,
    pub unknown_observations: u64,
    pub rate_card_refs: Vec<String>,
}
