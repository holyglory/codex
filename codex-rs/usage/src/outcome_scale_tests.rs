use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Instant;

/// Opt-in diagnostic uses an isolated backup, never a live collector.
#[tokio::test]
#[ignore = "requires an explicitly prepared isolated collector snapshot"]
async fn isolated_daily_window_scale_probe() {
    let directory = std::path::PathBuf::from(
        std::env::var("CXM_USAGE_SCALE_FIXTURE").expect("isolated fixture path"),
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("fixture-marker")).expect("fixture marker"),
        "Disposable isolated accounting verification copy. Never a production collector.\n"
    );
    let scopes: Value =
        serde_json::from_slice(&std::fs::read(directory.join("scope.json")).expect("scope file"))
            .expect("scopes");
    let store = UsageStore::open(&directory).await.expect("isolated store");
    let range = UtcTimeRange::new(
        scopes["window_start_ms"].as_i64().expect("start"),
        scopes["window_end_ms"].as_i64().expect("end"),
    )
    .expect("window");
    let mut evidence = Vec::new();
    for scope in scopes["repositories"].as_array().expect("repositories") {
        let query = PerformanceReviewQuery {
            repository_id: Some(
                RepositoryId::new(scope["repository_id"].as_str().expect("repository"))
                    .expect("id"),
            ),
            time_range: Some(range),
            ..Default::default()
        };
        let started = Instant::now();
        let packet = store
            .performance_review_packet(query)
            .await
            .expect("daily report");
        let elapsed_ms = started.elapsed().as_millis();
        let provider = packet.tokens.iter().find(|tokens| {
            tokens.category == "total_tokens" && tokens.provenance == "provider_reported"
        });
        assert_eq!(
            packet.outcomes.totals.provider_total_tokens.measured,
            provider.map_or(0, |tokens| tokens.measured_tokens)
        );
        assert_eq!(
            packet.outcomes.attributed.operations, 0,
            "historical operations must not gain outcomes"
        );
        evidence.push(json!({"repository":scope["name"], "elapsed_ms":elapsed_ms, "operations":packet.outcomes.totals.operations,
            "provider_tokens":packet.outcomes.totals.provider_total_tokens.measured, "coverage":packet.outcomes.coverage}));
    }
    std::fs::write(
        directory.join("native-window-proof.json"),
        serde_json::to_vec_pretty(&evidence).expect("evidence"),
    )
    .expect("write evidence");
    assert!(
        evidence
            .iter()
            .all(|row| row["elapsed_ms"].as_u64().expect("duration") <= 15_000),
        "{evidence:?}"
    );
}
