//! Additive cost counters shared by live reports and retention.
const COMPONENTS: &[(&str, &str)] = &[
    ("estimatedApiEquivalentCostMicros", "estimated"),
    ("inputCostMicros", "inputCost"),
    ("cachedInputCostMicros", "cachedInputCost"),
    ("cacheWriteCostMicros", "cacheWriteCost"),
    ("outputCostMicros", "outputCost"),
    ("reasoningCostMicros", "reasoningCost"),
];

pub(crate) fn provenance_type() -> &'static str {
    "STRUCT(model VARCHAR, provider VARCHAR, serviceTier VARCHAR, contextClass VARCHAR, rateContextClass VARCHAR,
    region VARCHAR, rateCard VARCHAR, rateCardVersion VARCHAR, pricingBasis VARCHAR, pricingCalculation VARCHAR,
    rateSourceUrl VARCHAR, rateSourceSha256 VARCHAR, pricedModel VARCHAR, unavailableReason VARCHAR,
    estimatedApiEquivalentCostMicros UBIGINT,inputCostMicros UBIGINT,cachedInputCostMicros UBIGINT,
    cacheWriteCostMicros UBIGINT,outputCostMicros UBIGINT,reasoningCostMicros UBIGINT,
    costCoverage STRUCT(estimatedUnknown UBIGINT,inputCostUnknown UBIGINT,cachedInputCostUnknown UBIGINT,
    cacheWriteCostUnknown UBIGINT,outputCostUnknown UBIGINT,reasoningCostUnknown UBIGINT,componentUnknown UBIGINT))"
}

pub(crate) fn trusted(expression: &str) -> String {
    // .12's capacity-based resolver was wrong; its persisted estimates must
    // never silently become trusted after upgrading. Preserve other provenance.
    format!("CASE WHEN {expression}.pricingBasis = 'api_equivalent_rate_card' AND
        {expression}.pricingCalculation IS DISTINCT FROM 'token-components-v2' THEN NULL ELSE {expression} END")
}

pub(crate) fn unknown(field: &str, counter: &str) -> String {
    format!(
        "COALESCE(provenance.costCoverage.{counter}Unknown,
        CASE WHEN provenance.{field} IS NULL THEN observations ELSE 0 END)"
    )
}

pub(crate) fn rollup(source: &str) -> String {
    let expression = format!(
        "TRY_CAST(TRY_CAST(cost_provenance AS JSON) AS {})",
        provenance_type()
    );
    format!(
        "SELECT * EXCLUDE (cost_provenance), {} AS provenance,
        COALESCE(observation_count,1)::UBIGINT observations FROM {source}",
        trusted(&expression)
    )
}

pub(crate) fn aggregated_json() -> String {
    let metadata = [
        "model",
        "provider",
        "serviceTier",
        "contextClass",
        "rateContextClass",
        "region",
        "rateCard",
        "rateCardVersion",
        "pricingBasis",
        "pricingCalculation",
        "rateSourceUrl",
        "rateSourceSha256",
        "pricedModel",
        "unavailableReason",
    ];
    let mut fields = metadata
        .iter()
        .map(|name| format!("{name} := provenance.{name}"))
        .collect::<Vec<_>>();
    let mut counters = Vec::new();
    for (field, counter) in COMPONENTS {
        fields.push(format!(
            "{field} := sum(provenance.{field}::HUGEINT)::UBIGINT"
        ));
        counters.push(format!(
            "{counter}Unknown := sum({})::UBIGINT",
            unknown(field, counter)
        ));
    }
    counters.push("componentUnknown := sum(COALESCE(provenance.costCoverage.componentUnknown, CASE WHEN provenance.inputCostMicros IS NULL OR provenance.cachedInputCostMicros IS NULL OR provenance.cacheWriteCostMicros IS NULL OR provenance.outputCostMicros IS NULL THEN observations ELSE 0 END))::UBIGINT".into());
    fields.push(format!(
        "costCoverage := struct_pack({})",
        counters.join(",")
    ));
    format!("to_json(struct_pack({}))::VARCHAR", fields.join(","))
}
