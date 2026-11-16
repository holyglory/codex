//! Keep the review projection's cost contract distinct from source provenance.
use serde_json::Value;
use serde_json::json;

pub(super) fn apply_costs(review: &mut Value, source: &Value, workstream: Option<&str>) {
    let cost = source_cost(source, &review["totals"]["model_requests"]);
    review["totals"]["cost"] = cost.clone();
    let outcomes = &mut review["outcomes"];
    outcomes["totals"]["cost"] = if workstream.is_none() {
        cost
    } else {
        unavailable_cost(&outcomes["totals"], "workstream_cost_unobserved")
    };
    for key in ["attributed", "unattributed"] {
        outcomes[key]["cost"] = unavailable_cost(&outcomes[key], "outcome_cost_unobserved");
    }
    if let Some(rows) = outcomes["rows"].as_array_mut() {
        for row in rows {
            row["effort"]["cost"] = unavailable_cost(&row["effort"], "outcome_cost_unobserved");
        }
    }
}

fn source_cost(source: &Value, model_requests: &Value) -> Value {
    json!({
        "status": source["status"], "basis": source["basis"], "currency": source["currency"],
        "processing_tier": source["processingTier"], "estimated_usd": source["estimatedUsd"],
        "estimated_usd_micros": source["estimatedUsdMicros"],
        "input_usd_micros": source["inputUsdMicros"],
        "cached_input_usd_micros": source["cachedInputUsdMicros"],
        "cache_write_usd_micros": source["cacheWriteUsdMicros"],
        "output_usd_micros": source["outputUsdMicros"],
        "input_tokens": source["inputTokens"], "cached_input_tokens": source["cachedInputTokens"],
        "cache_write_tokens": source["cacheWriteTokens"],
        "uncached_input_tokens": source["uncachedInputTokens"],
        "output_tokens": source["outputTokens"], "reasoning_tokens": source["reasoningTokens"],
        "unknown_requests": source["unknownRequests"], "unknown_tokens": source["unknownTokens"],
        "unknown_observations": source["unknownObservations"],
        "rate_card_ref": source["rateCardRefs"].as_array().and_then(|refs| refs.first()),
        "rate_card_refs": source["rateCardRefs"], "matched_rate_cards": [],
        "model_requests": model_requests, "priced_requests": source["pricedObservations"],
        "unavailable_reasons": source["unavailableReasons"],
    })
}

fn unavailable_cost(effort: &Value, reason: &str) -> Value {
    // Operation counts do not identify how many model requests or price
    // observations belong to this group. Keep their missing cost explicit.
    json!({
        "status":"unavailable", "basis":"api_equivalent", "currency":"USD",
        "processing_tier":"standard", "estimated_usd":null, "estimated_usd_micros":null,
        "input_usd_micros":null, "cached_input_usd_micros":null,
        "cache_write_usd_micros":null, "output_usd_micros":null,
        "input_tokens":null, "cached_input_tokens":null, "cache_write_tokens":null,
        "uncached_input_tokens":null, "output_tokens":null, "reasoning_tokens":null,
        "unknown_requests":0, "unknown_tokens":effort["providerTotalTokens"]["measured"],
        "unknown_observations":0, "rate_card_ref":null, "rate_card_refs":[],
        "matched_rate_cards":[], "model_requests":0, "priced_requests":0,
        "unavailable_reasons":{reason:1},
    })
}
