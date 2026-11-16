//! Bounded offline compaction and real-window rich-review timing.
use std::fs;
use std::path::Path;
use std::time::Instant;

use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use codex_usage::UsageReader;
use serde_json::Value;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let [root, scope, oracle, output] = args.as_slice() else {
        return Err("expected ROOT SCOPE_FILE ORACLE OUTPUT".into());
    };
    if !Path::new(root).join("probe-snapshot.json").is_file() {
        return Err("requires an offline diagnostic snapshot".into());
    }
    let manifest = fs::read(Path::new(root).join("manifest.json"))?;
    let started = Instant::now();
    let mut steps = 0;
    while codex_usage::compact_review_contributions(Path::new(root))? {
        steps += 1;
        if steps % 32 == 0 {
            println!(
                "{}",
                json!({"stage":"review_compaction","steps":steps,"elapsedMs":started.elapsed().as_secs_f64()*1000.0})
            );
        }
    }
    if fs::read(Path::new(root).join("manifest.json"))? != manifest {
        return Err("compaction modified canonical publication".into());
    }
    let compaction_ms = started.elapsed().as_secs_f64() * 1000.0;
    let oracle: Value = serde_json::from_slice(&fs::read(oracle)?)?;
    let scope: Value = serde_json::from_slice(&fs::read(scope)?)?;
    let filter = UsageFilter {
        repository_id: Some(
            scope["repositoryId"]
                .as_str()
                .ok_or("missing repository ID")?
                .into(),
        ),
        ..Default::default()
    };
    let from = oracle["fromAtMs"].as_i64().ok_or("missing oracle start")?;
    let to = oracle["toAtMs"].as_i64().ok_or("missing oracle end")?;
    let mut measurements = Vec::new();
    for attempt in 0..2 {
        let started = Instant::now();
        let reader = UsageReader::open_at(root)?;
        let opened = Instant::now();
        let window = reader.prepare_window(TimeRange::from_millis(from, to)?, &filter)?;
        let prepared = Instant::now();
        let review = window.performance_review()?;
        let ended = Instant::now();
        let total = review["tokens"]
            .as_array()
            .ok_or("missing token rows")?
            .iter()
            .find(|row| row["category"] == "total_tokens")
            .ok_or("missing provider total")?;
        if total["measuredTokens"] != oracle["windows"]["period"]["providerTotalTokens"] {
            return Err("rich review differs from raw provider-total oracle".into());
        }
        let value = json!({"attempt":attempt,"openMs":opened.duration_since(started).as_secs_f64()*1000.0,
            "prepareMs":prepared.duration_since(opened).as_secs_f64()*1000.0,
            "reviewMs":ended.duration_since(prepared).as_secs_f64()*1000.0,
            "elapsedMs":ended.duration_since(started).as_secs_f64()*1000.0,
            "matchesOracle":true,"counts":review["counts"],"providerTotal":total,
            "timing":review["timing"],"contextSize":review["contextSize"],"cost":review["cost"]});
        println!(
            "{}",
            json!({"stage":"rich_review","attempt":attempt,"elapsedMs":value["elapsedMs"],"matchesOracle":true})
        );
        measurements.push(value);
    }
    fs::write(
        output,
        serde_json::to_vec_pretty(
            &json!({"compactionSteps":steps,"compactionMs":compaction_ms,
        "generation":oracle["generation"],"watermark":oracle["watermark"],"measurements":measurements,
        "basis":"offline published snapshot; no work-binding side store; OS page cache not purged"}),
        )?,
    )?;
    Ok(())
}
