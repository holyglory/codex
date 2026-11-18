//! Versioned, local API-equivalent pricing; actual billing is never inferred.
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use crate::UsageCostProvenance;

pub const RATE_CARD_VERSION: &str = "openai-api-2026-11-18-v2";
pub const COST_CALCULATION: &str = "token-components-v2";

/// A verified provider boundary, not a user-editable display name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PricingProvider {
    OpenAiDirect,
    Other,
}

/// Request-scoped observations. Invalid or missing measurements remain unknown.
pub struct PricingRequest<'a> {
    pub model: &'a str,
    pub provider_name: &'a str,
    pub provider: PricingProvider,
    pub service_tier: Option<&'a str>,
    pub region: Option<&'a str>,
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub write_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub observed_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    version: String,
    source_url: String,
    source_sha256: String,
    context_threshold_tokens: i64,
    rows: Vec<Rate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rate {
    model: String,
    tier: String,
    context: String,
    input: u64,
    output: u64,
    cached: Option<u64>,
    #[serde(rename = "write30m")]
    write_30m: Option<u64>,
    #[serde(rename = "write12h")]
    write_12h: Option<u64>,
    valid_from_ms: i64,
    valid_until_ms: Option<i64>,
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("pricing/openai-2026-11-18.json"))
            .expect("checked embedded pricing catalog")
    })
}

/// Calculate once at response capture, with immutable rate and mapping provenance.
pub fn price_observation(
    request: PricingRequest<'_>,
    metadata: Option<&Value>,
) -> UsageCostProvenance {
    let mut result = UsageCostProvenance {
        model: Some(request.model.to_string()),
        provider: Some(request.provider_name.to_string()),
        service_tier: request.service_tier.map(str::to_owned),
        region: request.region.map(str::to_owned),
        pricing_calculation: Some(COST_CALCULATION.into()),
        pricing_basis: Some("api_equivalent_rate_card".into()),
        ..Default::default()
    };
    match calculate(&request, metadata, &mut result) {
        Ok(()) => {}
        Err(reason) => {
            result.unavailable_reason = Some(reason.into());
            result.estimated_api_equivalent_cost_micros = None;
            result.input_cost_micros = None;
            result.cached_input_cost_micros = None;
            result.cache_write_cost_micros = None;
            result.output_cost_micros = None;
            result.reasoning_cost_micros = None;
        }
    }
    result
}

fn calculate(
    request: &PricingRequest<'_>,
    metadata: Option<&Value>,
    result: &mut UsageCostProvenance,
) -> Result<(), &'static str> {
    if request.provider != PricingProvider::OpenAiDirect {
        return Err("unsupported_provider");
    }
    let observed = metadata.and_then(|v| v.get("codex_observed_response"));
    let model = observed
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .unwrap_or(request.model);
    let selected = observed
        .and_then(|v| v.get("service_tier"))
        .and_then(Value::as_str)
        .or(request.service_tier);
    // Exact provider namespace aliases only; no fuzzy family or date stripping.
    let canonical = model.strip_prefix("openai.").unwrap_or(model);
    result.model = Some(model.into());
    result.priced_model = Some(canonical.into());
    let tier = match selected.unwrap_or("default") {
        "default" | "standard" => "standard",
        "priority" | "fast" => "fast",
        "flex" => "flex",
        "batch" => "batch",
        "ultrafast" => "ultrafast",
        _ => return Err("unknown_service_tier"),
    };
    result.service_tier = Some(tier.into());
    if request
        .region
        .is_some_and(|region| !matches!(region, "" | "global"))
    {
        return Err("unsupported_region");
    }
    let inputs = u64::try_from(request.input_tokens).map_err(|_| "invalid_token_components")?;
    let cached = u64::try_from(request.cached_tokens).map_err(|_| "invalid_token_components")?;
    let writes = u64::try_from(request.write_tokens).map_err(|_| "invalid_token_components")?;
    let outputs = u64::try_from(request.output_tokens).map_err(|_| "invalid_token_components")?;
    let reasoning =
        u64::try_from(request.reasoning_tokens).map_err(|_| "invalid_token_components")?;
    let ordinary = inputs
        .checked_sub(cached)
        .and_then(|v| v.checked_sub(writes))
        .ok_or("invalid_token_components")?;
    if reasoning > outputs {
        return Err("invalid_token_components");
    }
    let card = catalog();
    let context = if request.input_tokens > card.context_threshold_tokens {
        "long"
    } else {
        "short"
    };
    result.context_class = Some(context.into());
    result.rate_context_class = Some(context.into());
    // Upstream explicit estimates are only usable with their source identity;
    // missing component prices do not become zero or a fabricated breakdown.
    if let Some(value) = metadata.filter(|v| v.is_object())
        && let Some(total) = value
            .get("estimatedApiEquivalentCostMicros")
            .or_else(|| value.get("estimated_api_equivalent_cost_micros"))
            .filter(|value| !value.is_null())
        {
            let total = total
                .as_u64()
                .or_else(|| total.as_str()?.parse().ok())
                .ok_or("provider_cost_invalid_amount")?;
            let id = string(value, "rateCard", "rate_card")
                .ok_or("provider_cost_provenance_incomplete")?;
            let version = string(value, "rateCardVersion", "rate_card_version")
                .ok_or("provider_cost_provenance_incomplete")?;
            let basis = string(value, "pricingBasis", "pricing_basis")
                .ok_or("provider_cost_provenance_incomplete")?;
            if basis != "api_equivalent" {
                return Err("provider_cost_basis_unsupported");
            }
            result.rate_card = Some(id.into());
            result.rate_card_version = Some(version.into());
            result.pricing_basis = Some("provider_api_equivalent".into());
            result.estimated_api_equivalent_cost_micros = Some(total);
            let components = [
                number(value, "inputCostMicros", "input_cost_micros"),
                number(value, "cachedInputCostMicros", "cached_input_cost_micros"),
                number(value, "cacheWriteCostMicros", "cache_write_cost_micros"),
                number(value, "outputCostMicros", "output_cost_micros"),
            ];
            if let [Some(a), Some(b), Some(c), Some(d)] = components {
                if a.checked_add(b)
                    .and_then(|v| v.checked_add(c))
                    .and_then(|v| v.checked_add(d))
                    != Some(total)
                {
                    return Err("provider_cost_components_inconsistent");
                }
                result.input_cost_micros = Some(a);
                result.cached_input_cost_micros = Some(b);
                result.cache_write_cost_micros = Some(c);
                result.output_cost_micros = Some(d);
                result.reasoning_cost_micros =
                    number(value, "reasoningCostMicros", "reasoning_cost_micros")
                        .filter(|r| *r <= d);
            }
            return Ok(());
        }
    let rate = card
        .rows
        .iter()
        .find(|row| {
            row.model == canonical
                && row.tier == tier
                && (row.context == context || row.context == "any")
        })
        .ok_or("rate_card_missing")?;
    if request.observed_at_ms < rate.valid_from_ms
        || rate
            .valid_until_ms
            .is_some_and(|end| request.observed_at_ms >= end)
    {
        return Err("rate_card_outside_validity");
    }
    let (write30, write12) = if writes == 0 {
        (0, 0)
    } else {
        let details = metadata
            .and_then(|v| v.get("input_tokens_details"))
            .and_then(|v| v.get("cache_write_tokens_details"))
            .ok_or("cache_write_duration_missing")?;
        let a = details
            .get("ttl_30m_tokens")
            .and_then(Value::as_u64)
            .ok_or("cache_write_duration_missing")?;
        let b = details
            .get("ttl_12h_tokens")
            .and_then(Value::as_u64)
            .ok_or("cache_write_duration_missing")?;
        if a.checked_add(b) != Some(writes) {
            return Err("invalid_cache_write_components");
        }
        (a, b)
    };
    // Products retain exact rational numerators until rounding each additive
    // component once (nearest micro, halves up); reasoning is a subset only.
    let input = charge(ordinary, Some(rate.input))?;
    let cache = charge(cached, rate.cached)?;
    let write = round(
        numerator(write30, rate.write_30m)?
            .checked_add(numerator(write12, rate.write_12h)?)
            .ok_or("cost_overflow")?,
    )?;
    let output = charge(outputs, Some(rate.output))?;
    let reason = charge(reasoning, Some(rate.output))?;
    let total = input
        .checked_add(cache)
        .and_then(|v| v.checked_add(write))
        .and_then(|v| v.checked_add(output))
        .ok_or("cost_overflow")?;
    result.rate_card = Some(format!(
        "{}:{canonical}:{tier}:{}",
        card.version, rate.context
    ));
    result.rate_card_version = Some(card.version.clone());
    result.rate_source_url = Some(card.source_url.clone());
    result.rate_source_sha256 = Some(card.source_sha256.clone());
    result.input_cost_micros = Some(input);
    result.cached_input_cost_micros = Some(cache);
    result.cache_write_cost_micros = Some(write);
    result.output_cost_micros = Some(output);
    result.reasoning_cost_micros = Some(reason);
    result.estimated_api_equivalent_cost_micros = Some(total);
    Ok(())
}

fn numerator(tokens: u64, rate: Option<u64>) -> Result<u128, &'static str> {
    if tokens == 0 {
        return Ok(0);
    }
    Ok(u128::from(tokens) * u128::from(rate.ok_or("token_category_unpriced")?))
}
fn round(value: u128) -> Result<u64, &'static str> {
    u64::try_from(value.checked_add(500_000).ok_or("cost_overflow")? / 1_000_000)
        .map_err(|_| "cost_overflow")
}
fn charge(tokens: u64, rate: Option<u64>) -> Result<u64, &'static str> {
    round(numerator(tokens, rate)?)
}
fn number(value: &Value, camel: &str, snake: &str) -> Option<u64> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}
fn string<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
#[path = "rate_card_tests.rs"]
mod tests;
