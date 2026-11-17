//! Trusted API-equivalent rate-card resolution.
//!
//! Rates are an immutable source snapshot. Provider/model aliases are mapped
//! only when the mapping is explicit; unknown providers, service tiers, and
//! model variants remain unpriced.

use crate::model::UsageCostProvenance;

/// Pricing snapshot used by the `.12` collector.
pub const RATE_CARD_VERSION: &str = "openai-api-pricing-2026-11-18";
const TOKENS_PER_MILLION: u128 = 1_000_000;
const LONG_CONTEXT_THRESHOLD: u64 = 272_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateCard {
    pub model: &'static str,
    pub service_tier: &'static str,
    pub context_class: &'static str,
    pub input_micros_per_million: u64,
    pub cached_input_micros_per_million: u64,
    pub cache_write_micros_per_million: Option<u64>,
    pub output_micros_per_million: u64,
}

impl RateCard {
    fn cost(self, tokens: u64, rate: u64) -> u64 {
        ((u128::from(tokens) * u128::from(rate)) / TOKENS_PER_MILLION)
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// Build a provenance record with component costs. Reasoning cost is a
    /// diagnostic subset of output cost and is deliberately not added twice.
    pub fn estimate(
        self,
        provider: &str,
        region: Option<&str>,
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
        context_class: String,
    ) -> UsageCostProvenance {
        let cached = cached_input_tokens.min(input_tokens);
        let uncached = input_tokens.saturating_sub(cached);
        let input_cost = self.cost(uncached, self.input_micros_per_million);
        let cached_cost = self.cost(cached, self.cached_input_micros_per_million);
        let cache_write_cost = self
            .cache_write_micros_per_million
            .map(|rate| self.cost(cache_write_tokens, rate));
        let output_cost = self.cost(output_tokens, self.output_micros_per_million);
        let reasoning_cost = self.cost(
            reasoning_tokens.min(output_tokens),
            self.output_micros_per_million,
        );
        let total = input_cost
            .saturating_add(cached_cost)
            .saturating_add(cache_write_cost.unwrap_or_default())
            .saturating_add(output_cost);
        UsageCostProvenance {
            model: Some(self.model.to_string()),
            provider: Some(provider.to_string()),
            service_tier: Some(self.service_tier.to_string()),
            context_class: Some(context_class),
            rate_context_class: Some(self.context_class.to_string()),
            region: region.map(str::to_owned),
            rate_card: Some(format!(
                "openai-api-{}-{}",
                self.service_tier, self.context_class
            )),
            rate_card_version: Some(RATE_CARD_VERSION.to_string()),
            pricing_basis: Some("api_equivalent_rate_card".to_string()),
            estimated_api_equivalent_cost_micros: Some(total),
            input_cost_micros: Some(input_cost),
            cached_input_cost_micros: Some(cached_cost),
            cache_write_cost_micros: cache_write_cost,
            output_cost_micros: Some(output_cost),
            reasoning_cost_micros: Some(reasoning_cost),
        }
    }
}

fn canonical_model(model: &str) -> Option<&'static str> {
    let model = model
        .strip_prefix("openai.")
        .or_else(|| model.strip_prefix("global.openai."))
        .or_else(|| model.strip_prefix("us.openai."))
        .unwrap_or(model);
    // Codex-specific variants are intentionally not silently priced as their
    // nearest API model. They need an explicit future mapping or metadata rate.
    match model {
        "gpt-6.2-astra" => Some("gpt-6.2-astra"),
        "gpt-6.2-sol" => Some("gpt-6.2-sol"),
        "gpt-6-astra" => Some("gpt-6-astra"),
        "gpt-6.1-sol" => Some("gpt-6.1-sol"),
        "gpt-6.1-terra" => Some("gpt-6.1-terra"),
        "gpt-6-luna" => Some("gpt-6-luna"),
        "gpt-6-sol" => Some("gpt-6-sol"),
        "gpt-5.6-sol" => Some("gpt-5.6-sol"),
        "gpt-5.6-terra" => Some("gpt-5.6-terra"),
        "gpt-5.6-luna" => Some("gpt-5.6-luna"),
        "gpt-5.5" => Some("gpt-5.5"),
        "gpt-5.4" => Some("gpt-5.4"),
        "gpt-5.4-mini" => Some("gpt-5.4-mini"),
        "gpt-5.4-nano" => Some("gpt-5.4-nano"),
        "gpt-5.2" => Some("gpt-5.2"),
        "gpt-5.1" => Some("gpt-5.1"),
        "gpt-5" => Some("gpt-5"),
        "gpt-5-mini" => Some("gpt-5-mini"),
        "gpt-5-nano" => Some("gpt-5-nano"),
        "gpt-4.1" => Some("gpt-4.1"),
        "gpt-4.1-mini" => Some("gpt-4.1-mini"),
        "gpt-4o" => Some("gpt-4o"),
        "gpt-4o-mini" => Some("gpt-4o-mini"),
        "o3" => Some("o3"),
        _ => None,
    }
}

fn service_tier(value: Option<&str>) -> Option<&'static str> {
    match value.unwrap_or("standard") {
        "standard" | "default" => Some("standard"),
        // OpenAI renamed priority processing to Fast; preserve the alias.
        "fast" | "priority" => Some("fast"),
        "flex" => Some("flex"),
        "batch" => Some("batch"),
        _ => None,
    }
}

/// Resolve a trusted current API-equivalent rate card for one observation.
pub fn resolve(
    model: &str,
    provider: &str,
    service_tier_value: Option<&str>,
    context_tokens: Option<u64>,
) -> Option<RateCard> {
    let provider = provider.to_ascii_lowercase();
    if !matches!(provider.as_str(), "openai" | "chatgpt" | "openai-codex") {
        return None;
    }
    let model = canonical_model(model)?;
    let tier = service_tier(service_tier_value)?;
    // Keep this table intentionally explicit. The standard short-context rates
    // are the primary supported path; long context is enabled where the source
    // pricing page publishes a distinct long-context rate.
    let long = context_tokens.is_some_and(|tokens| tokens > LONG_CONTEXT_THRESHOLD);
    let values = match (model, tier, long) {
        ("gpt-6.1-sol", "standard", false) => (2_000_000, 100_000, 2_500_000, 10_000_000),
        ("gpt-6.1-sol", "standard", true) => (4_000_000, 200_000, 5_000_000, 15_000_000),
        ("gpt-6.1-sol", "fast", false) => (4_000_000, 200_000, 5_000_000, 20_000_000),
        ("gpt-6.1-sol", "fast", true) => (8_000_000, 400_000, 10_000_000, 30_000_000),
        ("gpt-6.2-sol", "standard", false) => (2_000_000, 100_000, 2_500_000, 10_000_000),
        ("gpt-6.2-sol", "standard", true) => (4_000_000, 200_000, 5_000_000, 15_000_000),
        ("gpt-6.2-astra", "standard", false) => (8_000_000, 400_000, 10_000_000, 40_000_000),
        ("gpt-6.2-astra", "standard", true) => (16_000_000, 800_000, 20_000_000, 60_000_000),
        ("gpt-6-astra", "standard", false) => (10_000_000, 1_000_000, 12_500_000, 50_000_000),
        ("gpt-6-astra", "standard", true) => (20_000_000, 2_000_000, 25_000_000, 75_000_000),
        ("gpt-6.1-terra", "standard", false) => (1_000_000, 50_000, 1_250_000, 5_000_000),
        ("gpt-6.1-terra", "standard", true) => (2_000_000, 100_000, 2_500_000, 7_500_000),
        ("gpt-6-luna", "standard", false) => (100_000, 10_000, 125_000, 500_000),
        ("gpt-6-luna", "standard", true) => (200_000, 20_000, 250_000, 750_000),
        ("gpt-6-sol", "standard", false) => (2_000_000, 200_000, 2_500_000, 10_000_000),
        ("gpt-6-sol", "standard", true) => (4_000_000, 400_000, 5_000_000, 15_000_000),
        ("gpt-5.6-sol", "standard", false) => (4_000_000, 400_000, 5_000_000, 20_000_000),
        ("gpt-5.6-sol", "standard", true) => (8_000_000, 800_000, 10_000_000, 30_000_000),
        ("gpt-5.6-terra", "standard", false) => (2_000_000, 200_000, 2_500_000, 12_000_000),
        ("gpt-5.6-terra", "standard", true) => (4_000_000, 400_000, 5_000_000, 18_000_000),
        ("gpt-5.6-luna", "standard", false) => (200_000, 20_000, 250_000, 1_200_000),
        ("gpt-5.6-luna", "standard", true) => (400_000, 40_000, 500_000, 1_800_000),
        ("gpt-5.5", "standard", false) => (5_000_000, 500_000, 0, 30_000_000),
        ("gpt-5.5", "standard", true) => (10_000_000, 1_000_000, 0, 45_000_000),
        ("gpt-5.4", "standard", false) => (2_500_000, 250_000, 0, 15_000_000),
        ("gpt-5.4", "standard", true) => (5_000_000, 500_000, 0, 22_500_000),
        ("gpt-5.4-mini", "standard", false) => (750_000, 75_000, 0, 4_500_000),
        ("gpt-5.4-nano", "standard", false) => (200_000, 20_000, 0, 1_250_000),
        ("gpt-5.2", "standard", false) => (1_750_000, 175_000, 0, 14_000_000),
        ("gpt-5.1", "standard", false) => (1_250_000, 125_000, 0, 10_000_000),
        ("gpt-5", "standard", false) => (1_250_000, 125_000, 0, 10_000_000),
        ("gpt-5-mini", "standard", false) => (250_000, 25_000, 0, 2_000_000),
        ("gpt-5-nano", "standard", false) => (50_000, 5_000, 0, 400_000),
        ("gpt-4.1", "standard", false) => (2_000_000, 500_000, 0, 8_000_000),
        ("gpt-4.1-mini", "standard", false) => (400_000, 100_000, 0, 1_600_000),
        ("gpt-4o", "standard", false) => (2_500_000, 1_250_000, 0, 10_000_000),
        ("gpt-4o-mini", "standard", false) => (150_000, 75_000, 0, 600_000),
        ("o3", "standard", false) => (2_000_000, 500_000, 0, 8_000_000),
        _ => return None,
    };
    Some(RateCard {
        model,
        service_tier: tier,
        context_class: if long { "long" } else { "short" },
        input_micros_per_million: values.0,
        cached_input_micros_per_million: values.1,
        cache_write_micros_per_million: (values.2 != 0).then_some(values.2),
        output_micros_per_million: values.3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_provider_aliases_and_calculates_component_costs() {
        let card = resolve("global.openai.gpt-6.1-sol", "openai", None, Some(120_000))
            .expect("published standard card");
        let estimate = card.estimate(
            "openai",
            None,
            10_000,
            2_000,
            0,
            1_000,
            400,
            "within_window".into(),
        );
        assert_eq!(
            estimate.rate_card.as_deref(),
            Some("openai-api-standard-short")
        );
        assert_eq!(estimate.input_cost_micros, Some(16_000));
        assert_eq!(estimate.cached_input_cost_micros, Some(200));
        assert_eq!(estimate.output_cost_micros, Some(10_000));
        assert_eq!(estimate.reasoning_cost_micros, Some(4_000));
        assert_eq!(estimate.estimated_api_equivalent_cost_micros, Some(26_200));
    }

    #[test]
    fn leaves_external_and_unmapped_models_unknown() {
        assert!(resolve("gpt-5.3-codex", "openai", None, Some(1_000)).is_none());
        assert!(resolve("gpt-6.1-sol", "amazon-bedrock", None, Some(1_000)).is_none());
        assert!(resolve("gpt-6.1-sol", "openai", Some("priority"), Some(1_000)).is_some());
        assert!(resolve("gpt-5.4-mini", "openai", None, Some(300_000)).is_none());
    }
}
