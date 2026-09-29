//! Standard API-equivalent estimates, never subscription charges or invoices.
//! Rate-card snapshot: https://developers.openai.com/api/docs/pricing (2026-09-29).
//! Cache writes replace ordinary input pricing; reasoning is included in output.
use crate::UsageStoreError;
use serde::Serialize;
use sqlx::Row;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageApiEquivalentCost {
    pub basis: &'static str,
    pub currency: &'static str,
    pub status: &'static str,
    pub processing_tier: &'static str,
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

pub(super) fn aggregate(rows: Vec<sqlx::sqlite::SqliteRow>) -> Result<UsageApiEquivalentCost, UsageStoreError> {
    let mut totals = [0_u64; 7];
    let mut nanos = [0_u128; 4];
    let mut priced = 0_u64;
    let mut unknown = 0_u64;
    let mut refs = BTreeSet::new();
    for row in rows {
        let observations = u64::try_from(row.get::<i64,_>("observations")).map_err(|_| UsageStoreError::AggregateOverflow)?;
        let mut values = [0_u64; 7];
        for (index, name) in ["input_tokens", "uncached_input_tokens", "cached_input_tokens", "cache_write_tokens", "output_tokens", "reasoning_tokens", "total_tokens"].into_iter().enumerate() {
            values[index] = u64::try_from(row.try_get::<i64,_>(name).map_err(|_| UsageStoreError::AggregateOverflow)?).map_err(|_| UsageStoreError::AggregateOverflow)?;
            totals[index] = totals[index].checked_add(values[index]).ok_or(UsageStoreError::AggregateOverflow)?;
        }
        let model: String = row.get("model");
        let provider: String = row.get("provider_kind");
        let long_context: bool = row.get("long_context");
        let complete: bool = row.get("complete");
        let rates = if provider == "openai" && complete { rates(&model, long_context) } else { None };
        let Some(rates) = rates else {
            unknown = unknown.checked_add(observations).ok_or(UsageStoreError::AggregateOverflow)?;
            continue;
        };
        priced = priced.checked_add(observations).ok_or(UsageStoreError::AggregateOverflow)?;
        for (index, count) in [values[1],values[2],values[3],values[4]].into_iter().enumerate() {
            let value = u128::from(count).checked_mul(u128::from(rates[index])).ok_or(UsageStoreError::AggregateOverflow)?;
            nanos[index] = nanos[index].checked_add(value).ok_or(UsageStoreError::AggregateOverflow)?;
        }
        let context = if long_context {"long"} else {"short"};
        refs.insert(format!("openai-standard-2026-09-29:{model}:{context}"));
    }
    let mut micros = [None; 4];
    let mut estimated = None;
    if priced > 0 {
        let mut sum = 0_u64;
        for (index, value) in nanos.into_iter().enumerate() {
            // Round each aggregated component to the nearest USD micro exactly
            // once; never round per token or add overlapping token categories.
            let value = u64::try_from(value.checked_add(500).ok_or(UsageStoreError::AggregateOverflow)? / 1_000).map_err(|_| UsageStoreError::AggregateOverflow)?;
            micros[index] = Some(value);
            sum = sum.checked_add(value).ok_or(UsageStoreError::AggregateOverflow)?;
        }
        estimated = Some(sum);
    }
    Ok(UsageApiEquivalentCost {
        basis:"api_equivalent",currency:"USD",processing_tier:"standard",
        status:if priced == 0 {"unavailable"} else if unknown > 0 {"partial"} else {"complete"},
        estimated_usd_micros:estimated,input_usd_micros:micros[0],cached_input_usd_micros:micros[1],cache_write_usd_micros:micros[2],output_usd_micros:micros[3],
        input_tokens:totals[0],uncached_input_tokens:totals[1],cached_input_tokens:totals[2],cache_write_tokens:totals[3],output_tokens:totals[4],reasoning_tokens:totals[5],provider_total_tokens:totals[6],
        priced_observations:priced,unknown_observations:unknown,rate_card_refs:refs.into_iter().collect(),
    })
}

// Nanodollars per token. Exact model identities only: an unknown alias or
// provider is not priced using a similarly named model. Long context is >272K
// input tokens for the receipt, not cumulative input across a conversation.
fn rates(model: &str, long_context: bool) -> Option<[u64;4]> {
    let (short, long) = match model {
        "gpt-6-astra" => ([10_000,1_000,12_500,50_000],[20_000,2_000,25_000,75_000]),
        "gpt-6.1-sol" => ([2_000,100,2_500,10_000],[4_000,200,5_000,15_000]),
        "gpt-6-sol" => ([2_000,200,2_500,10_000],[4_000,400,5_000,15_000]),
        "gpt-6-luna" => ([100,10,125,500],[200,20,250,750]),
        "gpt-5.6-sol" => ([4_000,400,5_000,20_000],[8_000,800,10_000,30_000]),
        "gpt-5.6-terra" => ([2_000,200,2_500,12_000],[4_000,400,5_000,18_000]),
        "gpt-5.6-luna" => ([200,20,250,1_200],[400,40,500,1_800]),
        _ => return None,
    };
    Some(if long_context {long} else {short})
}
