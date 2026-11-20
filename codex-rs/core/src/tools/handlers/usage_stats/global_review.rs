//! Serve completed all-time global reviews without opening historical sources.
use super::*;
use codex_usage::GlobalReviewReader;

pub(super) fn selects(args: &UsageStatsArgs, filter: &UsageFilter) -> bool {
    matches!(args.action, UsageStatsAction::PerformanceReview)
        && matches!(args.scope, None | Some(UsageStatsScope::All))
        && args.from_at_ms.is_none_or(|from| from == i64::MIN)
        && args.to_at_ms.is_none_or(|to| to == i64::MAX)
        && args.agent_id.is_none()
        && args.root_thread_id.is_none()
        && *filter == UsageFilter::default()
}

pub(super) fn read(
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    if !matches!(args.limit, None | Some(1..=50)) {
        return Err(tool_error("limit must be between 1 and 50"));
    }
    let reader = GlobalReviewReader::open_at(context.codex_home.join(codex_usage::USAGE_DIRECTORY_NAME))
        .map_err(|_| tool_error("global performance review snapshot is unavailable; retry after background publication"))?
        .ok_or_else(|| tool_error("global performance review snapshot is not yet published (global_review_pending, retryable); retry after background publication"))?;
    let snapshot = reader.snapshot();
    let mut report = reader.report().clone();
    report["includeDescendants"] = json!(args.include_descendants.unwrap_or(false));
    let metadata = serde_json::Map::from_iter([
        (
            "snapshotGeneration".to_string(),
            json!(snapshot.source_snapshot_generation),
        ),
        ("watermark".to_string(), json!(snapshot.source_watermark)),
        (
            "journalWatermark".to_string(),
            json!(snapshot.included_journal_watermark),
        ),
        (
            "watermarkAtMs".to_string(),
            json!(snapshot.source_watermark_at_ms),
        ),
        (
            "pendingEntries".to_string(),
            json!(snapshot.pending_entries),
        ),
        (
            "pendingCorrections".to_string(),
            json!(snapshot.pending_corrections),
        ),
        ("freshness".to_string(), json!(snapshot.freshness)),
        (
            "sourceFreshness".to_string(),
            json!(snapshot.source_freshness),
        ),
        (
            "latestJournalWatermark".to_string(),
            json!(snapshot.latest_journal_watermark),
        ),
        ("sourceSnapshotId".to_string(), json!(snapshot.snapshot_id)),
        (
            "sourceWatermark".to_string(),
            json!(format!(
                "usage-v2:global-review:{}:g{}:w{}:j{}",
                snapshot.snapshot_id,
                snapshot.source_snapshot_generation,
                snapshot.source_watermark,
                snapshot.included_journal_watermark
            )),
        ),
        (
            "snapshotPublishedAtMs".to_string(),
            json!(snapshot.published_at_ms),
        ),
        (
            "sourceUpdatedAtMs".to_string(),
            json!(snapshot.source_updated_at_ms),
        ),
        ("projection".to_string(), json!("global_review_snapshot")),
        (
            "retention".to_string(),
            json!({
                "rolledUpObservations":snapshot.retired_observations,
                "unappliedCorrections":snapshot.unapplied_retention_corrections,
                "unappliedObservations":snapshot.unapplied_retention_records,
                "retiredReviewOperations":snapshot.retired_review_operations,
                "retiredReviewContributions":snapshot.retired_review_contributions,
                "totalsCoverage":if snapshot.unapplied_retention_corrections != 0 || snapshot.unapplied_retention_records != 0 {"partial_retention"} else {"recorded"},
                "historicalWindowCoverage":if snapshot.retired_observations == 0 {"rawObservations"} else {"wholeAggregateBuckets"},
            }),
        ),
    ]);
    Ok(with_metadata(report, &metadata))
}
