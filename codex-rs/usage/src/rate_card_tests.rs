use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn request() -> PricingRequest<'static> {
    PricingRequest {
        model: "gpt-6.1-sol",
        provider_name: "OpenAI",
        provider: PricingProvider::OpenAiDirect,
        service_tier: Some("standard"),
        region: None,
        input_tokens: 10_000,
        cached_tokens: 2_000,
        write_tokens: 0,
        output_tokens: 1_000,
        reasoning_tokens: 400,
        observed_at_ms: 1_795_000_000_000,
    }
}

#[test]
fn price_components_are_disjoint_and_reasoning_is_an_output_subset() {
    let result = price_observation(request(), None);
    assert_eq!(
        (
            result.input_cost_micros,
            result.cached_input_cost_micros,
            result.cache_write_cost_micros,
            result.output_cost_micros,
            result.reasoning_cost_micros,
            result.estimated_api_equivalent_cost_micros
        ),
        (
            Some(16_000),
            Some(200),
            Some(0),
            Some(10_000),
            Some(4_000),
            Some(26_200)
        )
    );
    let details = json!({"input_tokens_details":{"cache_write_tokens_details":{"ttl_30m_tokens":3000,"ttl_12h_tokens":4000}}});
    let result = price_observation(
        PricingRequest {
            input_tokens: 12000,
            cached_tokens: 4000,
            write_tokens: 7000,
            ..request()
        },
        Some(&details),
    );
    assert_eq!(
        (
            result.input_cost_micros,
            result.cached_input_cost_micros,
            result.cache_write_cost_micros,
            result.output_cost_micros,
            result.estimated_api_equivalent_cost_micros
        ),
        (
            Some(2_000),
            Some(400),
            Some(27_500),
            Some(10_000),
            Some(39_900)
        )
    );
}

#[test]
fn actual_context_threshold_and_served_model_and_tier_determine_rate() {
    for (input, context, price) in [(272_000, "short", 544_000), (272_001, "long", 1_088_004)] {
        let result = price_observation(
            PricingRequest {
                input_tokens: input,
                cached_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                ..request()
            },
            None,
        );
        assert_eq!(
            (
                result.rate_context_class.as_deref(),
                result.input_cost_micros
            ),
            (Some(context), Some(price))
        );
    }
    let observed =
        json!({"codex_observed_response":{"model":"gpt-6-astra","service_tier":"priority"}});
    let result = price_observation(request(), Some(&observed));
    assert_eq!(
        (
            result.model.as_deref(),
            result.service_tier.as_deref(),
            result.estimated_api_equivalent_cost_micros
        ),
        (Some("gpt-6-astra"), Some("fast"), Some(264_000))
    );
    for (tier, total) in [
        ("default", 26_200),
        ("priority", 52_400),
        ("flex", 13_100),
        ("batch", 13_100),
        ("ultrafast", 157_200),
    ] {
        assert_eq!(
            price_observation(
                PricingRequest {
                    service_tier: Some(tier),
                    ..request()
                },
                None
            )
            .estimated_api_equivalent_cost_micros,
            Some(total)
        );
    }
}

#[test]
fn unavailable_and_invalid_prices_never_become_saturated_or_zero_estimates() {
    let cases = [
        (
            PricingRequest {
                model: "made-up",
                ..request()
            },
            "rate_card_missing",
        ),
        (
            PricingRequest {
                provider: PricingProvider::Other,
                ..request()
            },
            "unsupported_provider",
        ),
        (
            PricingRequest {
                service_tier: Some("auto"),
                ..request()
            },
            "unknown_service_tier",
        ),
        (
            PricingRequest {
                cached_tokens: 10001,
                ..request()
            },
            "invalid_token_components",
        ),
        (
            PricingRequest {
                reasoning_tokens: 1001,
                ..request()
            },
            "invalid_token_components",
        ),
        (
            PricingRequest {
                input_tokens: -1,
                ..request()
            },
            "invalid_token_components",
        ),
        (
            PricingRequest {
                write_tokens: 1,
                ..request()
            },
            "cache_write_duration_missing",
        ),
        (
            PricingRequest {
                model: "gpt-5.6-sol",
                observed_at_ms: 1_796_112_000_000,
                ..request()
            },
            "rate_card_outside_validity",
        ),
        (
            PricingRequest {
                model: "gpt-6-astra",
                input_tokens: i64::MAX,
                output_tokens: i64::MAX,
                ..request()
            },
            "cost_overflow",
        ),
    ];
    for (input, reason) in cases {
        let result = price_observation(input, None);
        assert_eq!(
            (
                result.estimated_api_equivalent_cost_micros,
                result.unavailable_reason.as_deref()
            ),
            (None, Some(reason))
        );
    }
}

#[test]
fn zero_rounding_and_provider_estimates_preserve_their_distinct_provenance() {
    let zero = price_observation(
        PricingRequest {
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            ..request()
        },
        None,
    );
    assert_eq!(zero.estimated_api_equivalent_cost_micros, Some(0));
    let tiny = price_observation(
        PricingRequest {
            model: "gpt-6-luna",
            input_tokens: 6,
            cached_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            ..request()
        },
        None,
    );
    assert_eq!(tiny.input_cost_micros, Some(1));
    let metadata = json!({"rateCard":"provider-card","rateCardVersion":"1","pricingBasis":"api_equivalent",
        "estimatedApiEquivalentCostMicros":7,"inputCostMicros":1,"cachedInputCostMicros":1,"cacheWriteCostMicros":1,"outputCostMicros":4});
    let supplied = price_observation(request(), Some(&metadata));
    assert_eq!(
        (
            supplied.rate_card.as_deref(),
            supplied.estimated_api_equivalent_cost_micros
        ),
        (Some("provider-card"), Some(7))
    );
    assert_eq!(supplied.rate_source_url, None);
    for invalid in [json!(-1), json!(0.5), json!("invalid")] {
        let mut malformed = metadata.clone();
        malformed["estimatedApiEquivalentCostMicros"] = invalid;
        let rejected = price_observation(request(), Some(&malformed));
        assert_eq!(
            (
                rejected.estimated_api_equivalent_cost_micros,
                rejected.unavailable_reason.as_deref()
            ),
            (None, Some("provider_cost_invalid_amount"))
        );
    }
    let mismatch = json!({"estimatedApiEquivalentCostMicros":7});
    assert_eq!(
        price_observation(request(), Some(&mismatch))
            .unavailable_reason
            .as_deref(),
        Some("provider_cost_provenance_incomplete")
    );
}
