use crate::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn component_costs_survive_real_journal_publication_retention_and_restart() -> Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path().join(USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let at = 1_795_000_000_000i64;
    let pricing = |id: &str, input: i64| {
        let mut record = UsageRecord::new_millis(id, at, input as u64, 1000).with_thread("priced");
        record.cached_input_tokens = 2000;
        record.reasoning_output_tokens = 400;
        record.cache_write_input_tokens = Some(0);
        record.event = Some("token_observation".into());
        record.provider_total_tokens = Some(input as u64 + 1000);
        record.cost_provenance = Some(price_observation(
            PricingRequest {
                model: "gpt-6.1-sol",
                provider_name: "OpenAI",
                provider: PricingProvider::OpenAiDirect,
                service_tier: Some("priority"),
                region: None,
                input_tokens: input,
                cached_tokens: 2000,
                write_tokens: 0,
                output_tokens: 1000,
                reasoning_tokens: 400,
                observed_at_ms: at,
            },
            None,
        ));
        record
    };
    journal.append_record(pricing("a", 10000))?;
    journal.append_record(pricing("b", 12000))?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 32,
    })?;
    let filter = UsageFilter::default().for_thread("priced");
    let before = UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?;
    let mut maintenance = Default::default();
    for _ in 0..8 {
        crate::retention::maintain(
            &root,
            RetentionPolicy::default(),
            at + 12 * 86_400_000,
            &mut maintenance,
        )?;
    }
    let after = UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?;
    for report in [&before, &after] {
        assert_eq!(
            json!({"total":report["cost"]["estimatedUsdMicros"],"input":report["cost"]["inputUsdMicros"],
            "cached":report["cost"]["cachedInputUsdMicros"],"output":report["cost"]["outputUsdMicros"],
            "write":report["cost"]["cacheWriteUsdMicros"],"tier":report["cost"]["processingTier"],
            "unknown":report["cost"]["unknownObservations"],"priced":report["cost"]["pricedObservations"],"status":report["cost"]["status"]}),
            json!({"total":112800,"input":72000,"cached":800,"output":40000,"write":0,"tier":"fast","unknown":0,"priced":2,"status":"complete"})
        );
    }
    let mut invalid = pricing("old-invalid-v12", 10000);
    invalid.observed_at_ms = Some(at + 13 * 86_400_000);
    invalid.observed_at = (at + 13 * 86_400_000) / 1000;
    invalid
        .cost_provenance
        .as_mut()
        .unwrap()
        .pricing_calculation = None;
    journal.append_record(invalid)?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 32,
    })?;
    let mixed = UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?;
    assert_eq!(
        (
            mixed["cost"]["estimatedUsdMicros"].clone(),
            mixed["cost"]["unknownObservations"].clone(),
            mixed["cost"]["status"].clone()
        ),
        (json!(112800), json!(1), json!("partial"))
    );
    Ok(())
}
