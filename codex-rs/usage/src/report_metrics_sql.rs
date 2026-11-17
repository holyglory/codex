//! Aggregate complete metric sources before returning bounded report detail.

const MAX_DETAIL_ROWS: usize = 32;

pub(super) fn interval_rows_query(operations: &str, start_ms: i64, end_ms: i64) -> String {
    format!(
        "SELECT timing_group, observation_weight, COALESCE(kind, 'unknown') kind, started_at_ms, ended_at_ms,
                COALESCE(agent_id, thread_id) AS interval_owner,
                greatest(started_at_ms::HUGEINT, {start_ms}) clipped_start,
                least(ended_at_ms::HUGEINT, {end_ms}) clipped_end,
                CASE WHEN retired_interval THEN 'retired_interval'
                     WHEN started_at_ms IS NULL THEN 'missing_start'
                     WHEN ended_at_ms IS NULL THEN 'missing_end'
                     WHEN ended_at_ms < started_at_ms THEN 'invalid_interval' END reason
            FROM ({operations})"
    )
}

pub(super) fn grouped_interval_query(operations: &str, start_ms: i64, end_ms: i64) -> String {
    let intervals = interval_rows_query(operations, start_ms, end_ms);
    format!(
        "WITH intervals AS MATERIALIZED ({intervals}), valid_intervals AS (
            SELECT * FROM intervals WHERE reason IS NULL AND clipped_start < clipped_end
        ), spans AS (
            SELECT timing_group, 'wait' metric, '' AS interval_owner, clipped_start, clipped_end
                FROM valid_intervals WHERE kind = 'wait'
            UNION ALL
            SELECT timing_group, 'elapsed', '', clipped_start, clipped_end FROM valid_intervals WHERE kind <> 'wait'
            UNION ALL
            SELECT timing_group, 'active', interval_owner, clipped_start, clipped_end
                FROM valid_intervals WHERE kind <> 'wait' AND interval_owner IS NOT NULL
        ), covered AS (
            SELECT *, max(clipped_end) OVER (
                PARTITION BY timing_group, metric, interval_owner ORDER BY clipped_start, clipped_end
                ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
            ) covered_end FROM spans
        ), measured AS (
            SELECT timing_group, metric,
                sum(greatest(clipped_end - greatest(clipped_start,
                    COALESCE(covered_end, clipped_start)), 0))::UBIGINT ms
            FROM covered GROUP BY timing_group, metric
        ), unknowns AS (
            SELECT timing_group, kind || ':' || reason AS reason_key, sum(observation_weight)::UBIGINT observations FROM (
                SELECT timing_group, kind, reason, observation_weight FROM intervals WHERE reason IS NOT NULL
                UNION ALL
                SELECT timing_group, kind, 'missing_active_owner', observation_weight FROM valid_intervals
                    WHERE kind <> 'wait' AND interval_owner IS NULL
            ) GROUP BY ALL
        ), summaries AS (
        SELECT timing_group, sum(observation_weight)::UBIGINT operations,
            COALESCE(sum(observation_weight) FILTER (WHERE kind <> 'wait'),0)::UBIGINT execution_observations,
            COALESCE(sum(observation_weight) FILTER (WHERE kind = 'wait'),0)::UBIGINT wait_observations,
            min(started_at_ms) FILTER (WHERE reason IS NULL) first_started_at_ms,
            max(ended_at_ms) FILTER (WHERE reason IS NULL) last_ended_at_ms,
            COALESCE(sum(observation_weight) FILTER (WHERE kind = 'wait' AND reason IS NOT NULL),0)::UBIGINT wait_unknown,
            COALESCE(sum(observation_weight) FILTER (WHERE kind <> 'wait' AND (reason IS NOT NULL OR
                (interval_owner IS NULL AND clipped_start < clipped_end))),0)::UBIGINT elapsed_unknown,
            COALESCE(sum(observation_weight) FILTER (WHERE kind <> 'wait' AND (reason IS NOT NULL OR
                (interval_owner IS NULL AND clipped_start < clipped_end))),0)::UBIGINT active_unknown
        FROM intervals GROUP BY timing_group
        ), measurements AS (
            SELECT timing_group,
                COALESCE(sum(ms) FILTER (WHERE metric='active'),0)::UBIGINT active_ms,
                COALESCE(sum(ms) FILTER (WHERE metric='elapsed'),0)::UBIGINT elapsed_ms,
                COALESCE(sum(ms) FILTER (WHERE metric='wait'),0)::UBIGINT wait_ms
            FROM measured GROUP BY timing_group
        ), reasons AS (
            SELECT timing_group, json_group_object(reason_key,observations) unknown_reasons
            FROM unknowns GROUP BY timing_group
        )
        SELECT summaries.*, COALESCE(active_ms,0)::UBIGINT active_ms,
            COALESCE(elapsed_ms,0)::UBIGINT elapsed_ms, COALESCE(wait_ms,0)::UBIGINT wait_ms,
            COALESCE(unknown_reasons,'{{}}'::JSON) unknown_reasons
        FROM summaries LEFT JOIN measurements USING(timing_group) LEFT JOIN reasons USING(timing_group)"
    )
}

pub(super) fn context_and_cost_query(contributions: &str) -> String {
    let mut aggregates = vec![sum("observations", "observations")];
    for (field, measured, unknown) in [
        ("promptTokens", "prompt_tokens", "prompt_unknown"),
        ("contextTokens", "context_tokens", "context_unknown"),
        (
            "contextWindowTokens",
            "context_window_tokens",
            "context_window_unknown",
        ),
        ("cacheTokens", "cache_tokens", "cache_unknown"),
        ("compactionCount", "compaction_count", "compaction_unknown"),
    ] {
        aggregates.push(sum(&format!("context.{field}"), measured));
        aggregates.push(sum(
            &format!("CASE WHEN context.{field} IS NULL THEN observations ELSE 0 END"),
            unknown,
        ));
    }
    for (source, alias) in [
        ("input_tokens", "input_tokens"),
        ("cached_input_tokens", "cached_input_tokens"),
        ("output_tokens", "output_tokens"),
        ("reasoning_output_tokens", "reasoning_tokens"),
    ] {
        aggregates.push(format!(
            "COALESCE(sum({source}::HUGEINT),0)::BIGINT {alias}"
        ));
    }
    for (source, alias) in [
        ("cache_write_input_tokens", "cache_write_tokens"),
        ("provider_total_tokens", "provider_total_tokens"),
        ("provenance.estimatedApiEquivalentCostMicros", "estimated"),
        ("actual_billing_micros", "actual"),
        ("provenance.inputCostMicros", "input_cost"),
        ("provenance.cachedInputCostMicros", "cached_input_cost"),
        ("provenance.cacheWriteCostMicros", "cache_write_cost"),
        ("provenance.outputCostMicros", "output_cost"),
        ("provenance.reasoningCostMicros", "reasoning_cost"),
    ] {
        aggregates.push(sum(source, alias));
    }
    for (field, alias) in [
        ("context.windowClass", "window_unknown"),
        (
            "provenance.estimatedApiEquivalentCostMicros",
            "estimated_unknown",
        ),
    ] {
        aggregates.push(sum(
            &format!("CASE WHEN {field} IS NULL THEN observations ELSE 0 END"),
            alias,
        ));
    }
    // A weighted billing sum does not retain each receipt's availability.
    // Preserve its measured amount without claiming exact individual coverage.
    aggregates.push(sum(
        "CASE WHEN observations <> 1 OR actual_billing_micros IS NULL THEN observations ELSE 0 END",
        "actual_unknown",
    ));
    aggregates.push(sum(
        "CASE WHEN provenance.estimatedApiEquivalentCostMicros IS NOT NULL THEN observations ELSE 0 END",
        "priced_observations",
    ));
    aggregates.push(sum(
        "CASE WHEN provenance.inputCostMicros IS NULL OR provenance.outputCostMicros IS NULL THEN observations ELSE 0 END",
        "component_unknown",
    ));
    let aggregates = aggregates.join(", ");
    let predicate = crate::review_aggregate::TOKEN_PREDICATE;
    // A retained row can carry one representative JSON object for many facts.
    // Its additive token/billing columns remain useful; that JSON is unknown.
    format!(
        "WITH token_rows AS MATERIALIZED (
            SELECT COALESCE(observation_count, 1)::UBIGINT observations,
                input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens,
                reasoning_output_tokens, provider_total_tokens, actual_billing_micros,
                CASE WHEN COALESCE(observation_count, 1) = 1 THEN
                    TRY_CAST(TRY_CAST(context_size AS JSON) AS STRUCT(
                        promptTokens UBIGINT, contextTokens UBIGINT, cacheTokens UBIGINT,
                        compactionCount UBIGINT, contextWindowTokens UBIGINT, windowClass VARCHAR
                    )) END context,
                CASE WHEN COALESCE(observation_count, 1) = 1 THEN
                    TRY_CAST(TRY_CAST(cost_provenance AS JSON) AS STRUCT(
                        model VARCHAR, provider VARCHAR, serviceTier VARCHAR, contextClass VARCHAR, rateContextClass VARCHAR,
                        region VARCHAR, rateCard VARCHAR, rateCardVersion VARCHAR, pricingBasis VARCHAR,
                        estimatedApiEquivalentCostMicros UBIGINT, inputCostMicros UBIGINT,
                        cachedInputCostMicros UBIGINT, cacheWriteCostMicros UBIGINT,
                        outputCostMicros UBIGINT, reasoningCostMicros UBIGINT
                    )) END provenance
            FROM ({contributions}) WHERE {predicate}
        ), totals AS (
            SELECT {aggregates} FROM token_rows
        ), windows AS MATERIALIZED (
            SELECT context.windowClass AS window_class,
                least(sum(observations), 18446744073709551615)::UBIGINT observations
                FROM token_rows WHERE context.windowClass IS NOT NULL GROUP BY ALL
        ), provenances AS MATERIALIZED (
            SELECT provenance, least(sum(observations), 18446744073709551615)::UBIGINT observations,
                to_json(provenance)::VARCHAR sort_key
                FROM token_rows WHERE provenance IS NOT NULL GROUP BY ALL
        ), cards AS MATERIALIZED (
            SELECT CASE WHEN provenance.rateCardVersion IS NULL THEN provenance.rateCard
                ELSE provenance.rateCard || '@' || provenance.rateCardVersion END reference,
                least(sum(observations), 18446744073709551615)::UBIGINT observations
                FROM token_rows WHERE provenance.rateCard IS NOT NULL GROUP BY ALL
        )
        SELECT totals.*,
            COALESCE((SELECT to_json(list(struct_pack(\"class\" := window_class, observations := observations)
                ORDER BY window_class)) FROM (SELECT * FROM windows ORDER BY window_class LIMIT {MAX_DETAIL_ROWS})),
                '[]'::JSON) window_classes,
            (SELECT greatest(count(*) - {MAX_DETAIL_ROWS}, 0)::UBIGINT FROM windows)
                window_classes_omitted,
            (SELECT least(COALESCE(sum(observations), 0), 18446744073709551615)::UBIGINT FROM (
                SELECT * FROM windows ORDER BY window_class OFFSET {MAX_DETAIL_ROWS}))
                window_class_observations_omitted,
            COALESCE((SELECT to_json(list(struct_pack(
                model := provenance.model, provider := provenance.provider,
                serviceTier := provenance.serviceTier, contextClass := provenance.contextClass,
                rateContextClass := provenance.rateContextClass,
                region := provenance.region, rateCard := provenance.rateCard,
                rateCardVersion := provenance.rateCardVersion, pricingBasis := provenance.pricingBasis,
                estimatedApiEquivalentCostMicros := provenance.estimatedApiEquivalentCostMicros,
                inputCostMicros := provenance.inputCostMicros,
                cachedInputCostMicros := provenance.cachedInputCostMicros,
                cacheWriteCostMicros := provenance.cacheWriteCostMicros,
                outputCostMicros := provenance.outputCostMicros,
                reasoningCostMicros := provenance.reasoningCostMicros,
                observations := observations) ORDER BY sort_key)) FROM (
                    SELECT * FROM provenances ORDER BY sort_key LIMIT {MAX_DETAIL_ROWS})),
                '[]'::JSON) provenance,
            (SELECT greatest(count(*) - {MAX_DETAIL_ROWS}, 0)::UBIGINT FROM provenances)
                provenance_omitted,
            (SELECT least(COALESCE(sum(observations), 0), 18446744073709551615)::UBIGINT FROM (
                SELECT * FROM provenances ORDER BY sort_key OFFSET {MAX_DETAIL_ROWS}))
                provenance_observations_omitted,
            COALESCE((SELECT to_json(list(reference ORDER BY reference)) FROM (
                SELECT * FROM cards ORDER BY reference LIMIT {MAX_DETAIL_ROWS})), '[]'::JSON)
                rate_card_refs,
            (SELECT greatest(count(*) - {MAX_DETAIL_ROWS}, 0)::UBIGINT FROM cards)
                rate_card_refs_omitted,
            (SELECT least(COALESCE(sum(observations), 0), 18446744073709551615)::UBIGINT FROM (
                SELECT * FROM cards ORDER BY reference OFFSET {MAX_DETAIL_ROWS}))
                rate_card_observations_omitted
        FROM totals"
    )
}

fn sum(expression: &str, alias: &str) -> String {
    format!("COALESCE(sum({expression}), 0)::UBIGINT {alias}")
}
