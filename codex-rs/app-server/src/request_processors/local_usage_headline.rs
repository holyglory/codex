//! Exact published token headlines without opening rich operation projections.
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LocalUsageSnapshot;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_usage::Freshness;
use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use codex_usage::UsageHeadlineReader;
use codex_usage::UsageHeadlineSnapshot;
use codex_usage::UsageHeadlineTotals;
use serde_json::Value;
use serde_json::json;

use super::local_usage_leases::Lease;
use super::local_usage_leases::LeaseCache;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const RETENTION: Duration = Duration::from_secs(5);
const MAX_VIEWS: usize = 16;
static CACHE: LazyLock<Arc<LeaseCache<View>>> =
    LazyLock::new(|| LeaseCache::new(MAX_VIEWS, RETENTION));
static SERIAL: AtomicU64 = AtomicU64::new(1);

struct View {
    captured_at_ms: u64,
    watermark: String,
    reader: UsageHeadlineReader,
}

pub(super) fn read(
    home: PathBuf,
    params: LocalUsageSummaryParams,
) -> Result<LocalUsageSummaryResponse, JSONRPCErrorError> {
    let view = open(&home, params.expected_snapshot.as_deref())?;
    let range = match params.from_at_ms.zip(params.to_at_ms) {
        Some((from, to)) => TimeRange::from_millis(from, to)
            .map_err(|_| invalid_params("invalid local usage time range"))?,
        None => TimeRange::all(),
    };
    let filter = UsageFilter {
        account_id: params.account_id.clone(),
        repository_id: params.repository_id.clone(),
        thread_id: params.thread_id.clone(),
        ..Default::default()
    };
    let totals = view
        .reader
        .totals(range, &filter)
        .map_err(|_| unavailable("token_headline_unavailable"))?
        .ok_or_else(|| {
            let mut error = internal_error(
                "exact local usage tokens unavailable at this retained time boundary",
            );
            error.data = Some(json!({"source":"usage-v2","coverage":"unavailable",
                "reason":"token_headline_exact_window_unavailable","retryable":false}));
            error
        })?;
    let source = view.reader.snapshot();
    let snapshot = LocalUsageSnapshot {
        source_watermark: view.watermark.clone(),
        generated_at: view.captured_at_ms,
        generation: source.source_snapshot_generation,
        watermark: source.source_watermark.sequence,
        journal_watermark: source.source_watermark.sequence,
        watermark_at_ms: source.source_watermark_at_ms,
        updated_at_ms: source.published_at_ms,
        freshness: match source.freshness {
            Freshness::Fresh => "fresh",
            Freshness::Stale => "stale",
            Freshness::Unknown => "unknown",
        }
        .into(),
        pending_entries: source.pending_entries,
        pending_corrections: source.pending_corrections,
        refresh_id: None,
    };
    let report = report(&params, &totals, source);
    let observed = totals.token_observations > 0;
    let response = LocalUsageSummaryResponse {
        contract_version: 1,
        source: "usage-v2".into(),
        generated_at: super::local_usage_processor::now_ms()?,
        snapshot,
        report,
        summary: json!({"projection":"token_headline","counts":Value::Null,
            "inputTokens":totals.input_tokens,"cachedInputTokens":totals.cached_input_tokens,
            "cacheWriteInputTokens":totals.cache_write_input_tokens.filter(|_| observed),"outputTokens":totals.output_tokens,
            "reasoningOutputTokens":totals.reasoning_output_tokens,"providerTotalTokens":totals.provider_total_tokens.filter(|_| observed),
            "tokenObservations":totals.token_observations}),
        review: None,
        observations: None,
        next_records_cursor: None,
        operations: None,
        next_operations_cursor: None,
        work_contexts: Vec::new(),
        operation_bindings: Vec::new(),
        context_size: None,
        cost_provenance: None,
        lifecycle: None,
        timing: None,
    };
    view.retain_after_response()
        .map_err(super::local_usage_leases::rpc_error)?;
    Ok(response)
}

fn open(home: &Path, expected: Option<&str>) -> Result<Lease<View>, JSONRPCErrorError> {
    if let Some(expected) = expected {
        return CACHE
            .existing(home, expected)
            .map_err(super::local_usage_leases::rpc_error);
    }
    let reservation = CACHE
        .reserve()
        .map_err(super::local_usage_leases::rpc_error)?;
    let reader = UsageHeadlineReader::open_at(home.join(codex_usage::USAGE_DIRECTORY_NAME))
        .map_err(|_| unavailable("token_headline_unavailable"))?
        .ok_or_else(|| unavailable("token_headline_pending"))?;
    let captured_at_ms = super::local_usage_processor::now_ms()?;
    let watermark = format!(
        "usage-v2:headline:{}-{}:g{}:w{}",
        reader.snapshot().snapshot_id,
        SERIAL.fetch_add(1, Ordering::Relaxed),
        reader.snapshot().source_snapshot_generation,
        reader.snapshot().source_watermark.sequence,
    );
    let view = View {
        captured_at_ms,
        watermark: watermark.clone(),
        reader,
    };
    reservation
        .insert(home.to_owned(), watermark, view)
        .map_err(super::local_usage_leases::rpc_error)
}

fn report(
    params: &LocalUsageSummaryParams,
    totals: &UsageHeadlineTotals,
    source: &UsageHeadlineSnapshot,
) -> Value {
    let (scope, id) = if let Some(id) = &params.repository_id {
        ("repository", Some(id))
    } else if let Some(id) = &params.thread_id {
        ("thread", Some(id))
    } else if let Some(id) = &params.account_id {
        ("account", Some(id))
    } else {
        ("all", None)
    };
    let state = if totals.provider_total_unknowns != 0 || totals.missing_observations != 0 {
        "partial"
    } else if totals.token_observations == 0 {
        "unobserved"
    } else {
        "complete"
    };
    let tokens = [
        (
            "total_tokens",
            json!(totals.known_provider_total_tokens),
            totals.provider_total_unknowns,
        ),
        ("input_tokens", json!(totals.input_tokens), 0),
        (
            "input_tokens_details.cached_tokens",
            json!(totals.cached_input_tokens),
            0,
        ),
        ("output_tokens", json!(totals.output_tokens), 0),
        (
            "output_tokens_details.reasoning_tokens",
            json!(totals.reasoning_output_tokens),
            0,
        ),
        (
            "cache_write_input_tokens",
            json!(totals.known_cache_write_input_tokens),
            totals.cache_write_unknowns,
        ),
    ]
    .into_iter()
    .map(|(category, measured, unknown)| {
        json!({"category":category,
        "repositoryBucket":params.repository_id.as_deref().unwrap_or("all"),
        "measurementProvenance":"provider_reported","measuredTokens":measured,
        "exactTokens":(unknown == 0 && totals.token_observations > 0).then_some(measured),
        "unknownObservations":unknown,"observationCount":totals.token_observations})
    })
    .collect::<Vec<_>>();
    json!({"schemaVersion":1,"kind":"usageSummary","projection":"token_headline",
        "databaseSchemaVersion":codex_usage::MANIFEST_FORMAT_VERSION,"taxonomyVersion":codex_usage::USAGE_TAXONOMY_VERSION,
        "scope":{"type":scope,"id":id},"timeRange":params.from_at_ms.zip(params.to_at_ms).map(|(from,to)|json!({"startMs":from,"endMs":to})),
        "counts":Value::Null,"providerTokens":tokens,"providerTokensByActivity":Value::Null,
        "coverage":{"state":state,"hasGaps":state != "complete","providerTotals":if totals.provider_total_unknowns == 0 {"recorded"} else {"partial"},
            "counts":"deferred","cost":"deferred","timing":"deferred","lifecycle":"deferred","contextSize":"deferred","workBindings":"deferred","activity":"deferred",
            "details":{"basis":"published_snapshot","exactAtPublishedWatermark":true,"tokenObservations":totals.token_observations,
                "missingTokenObservations":totals.missing_observations,"providerTotalUnknowns":totals.provider_total_unknowns,"cacheWriteUnknowns":totals.cache_write_unknowns,
                "sourceSnapshotId":source.snapshot_id,"sourceFreshness":source.source_freshness,"latestJournalWatermark":source.latest_journal_watermark,
                "retiredObservations":source.retired_observations,"rawAdmissionCutoffAtMs":source.raw_admission_cutoff_at_ms,
                "unappliedRetentionRecords":source.unapplied_retention_records,"unappliedRetentionCorrections":source.unapplied_retention_corrections}},
        "timing":Value::Null,"lifecycle":Value::Null,"contextSize":Value::Null,"cost":Value::Null,"measuredExecutionIntervalSumMs":Value::Null,"workBindings":Value::Null,
        "filters":{"accountId":params.account_id,"repositoryId":params.repository_id,"threadId":params.thread_id}})
}

fn unavailable(reason: &str) -> JSONRPCErrorError {
    let mut error = internal_error("local usage token headline unavailable");
    error.data = Some(
        json!({"source":"usage-v2","coverage":"unavailable","reason":reason,"retryable":true}),
    );
    error
}
