use std::collections::BTreeMap;
use std::collections::BTreeSet;

use duckdb::Connection;
use serde_json::Value;
use serde_json::json;

use crate::Result;
use crate::ReviewCountScope;
use crate::ReviewCounts;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::query_error;
use crate::reader_query::quoted;
use crate::report_query::ReportQuery;
use crate::report_query::canonical_tail;
use crate::review_counts::read_review_counts;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_projection::load_review_operations;
use crate::review_projection::load_review_projection_shared as load_review_projection;
use crate::review_state::ReviewClassification;
use crate::review_state::ReviewOperation;
use crate::review_state::lifecycle;
use crate::review_storage::parquet_source;
use crate::review_storage::query_rows;

impl UsageReader {
    /// Read exact token components and canonical operation counts. All-time
    /// global/account/repository/thread queries use precomputed metadata plus
    /// bounded recent facts; they do not aggregate historical operation rows.
    pub fn summary(&self, range: TimeRange, filter: &UsageFilter) -> Result<Value> {
        if let Some(cached) = crate::summary_cache::get(
            self.root.clone(),
            self.metadata_revision,
            self.manifest.snapshot_generation,
            self.manifest.watermark,
            self.journal_watermark(),
            self.pending_entries(),
            self.manifest.pending_corrections.len(),
            range,
            filter,
        ) {
            return Ok(cached);
        }
        let totals = self.totals(range, filter)?;
        let descriptor = load_review_projection(&self.root, &self.manifest)?;
        let complete_from = descriptor
            .as_ref()
            .map_or(self.manifest.watermark.saturating_add(1), |value| {
                value.complete_from_sequence
            });
        let scope = count_scope(filter);
        let counts = if range == TimeRange::all()
            && let Some(scope) = scope
        {
            let key = count_scope_key(&scope);
            let base = self
                .manifest
                .review_counts
                .get(&key)
                .filter(|_| self.manifest.review_counts_watermark == Some(self.manifest.watermark))
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| {
                    descriptor.as_ref().map_or_else(
                        || Ok(ReviewCounts::default()),
                        |descriptor| read_review_counts(&self.root, descriptor, &scope),
                    )
                })?;
            self.count_tail(descriptor.as_deref(), filter, base)?
        } else {
            let query = ReportQuery::new(self, range, filter)?;
            query_rows::<ReviewCounts>(&query.connection, &format!(
                "SELECT COALESCE(sum(operation_count),0)::UBIGINT operations, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind = 'model_request'),0)::UBIGINT model_requests, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind = 'tool'),0)::UBIGINT tools, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind = 'model_warmup'),0)::UBIGINT warmups, \
                 COALESCE(sum(unfinished),0)::UBIGINT unfinished, \
                 COALESCE(sum(unknown_intervals),0)::UBIGINT unknown_intervals, \
                 COALESCE(sum(operation_count) FILTER (WHERE phase IS NULL OR activity IS NULL),0)::UBIGINT unclassified, \
                 COALESCE(sum(operation_count) FILTER (WHERE repository_id IS NULL),0)::UBIGINT missing_repository, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind='tool' AND outcome='completed'),0)::UBIGINT tool_completed, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind='tool' AND outcome='failed'),0)::UBIGINT tool_failed, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind='tool' AND outcome='blocked'),0)::UBIGINT tool_blocked, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind='tool' AND outcome='cancelled'),0)::UBIGINT tool_cancelled, \
                 COALESCE(sum(operation_count) FILTER (WHERE kind='tool' AND outcome='incomplete'),0)::UBIGINT tool_incomplete FROM ({}) WHERE kind <> 'wait'", query.operation_statistics
            ), 1)?.remove(0)
        };
        let value = json!({
            "schemaVersion":1,"kind":"usageSummary", "records":totals.records,
            "inputTokens":totals.input_tokens,"cachedInputTokens":totals.cached_input_tokens,
            "outputTokens":totals.output_tokens,"reasoningOutputTokens":totals.reasoning_output_tokens,
            "counts":{"operations":counts.operations,"modelRequests":counts.model_requests,"tools":counts.tools,
                "warmups":counts.warmups,"unfinished":counts.unfinished,"unknownIntervals":counts.unknown_intervals,
                "unclassified":counts.unclassified,"missingRepository":counts.missing_repository},
            "tools":{"count":counts.tools,"outcomes":[
                {"outcome":"completed","count":counts.tool_completed},{"outcome":"failed","count":counts.tool_failed},
                {"outcome":"blocked","count":counts.tool_blocked},{"outcome":"cancelled","count":counts.tool_cancelled},
                {"outcome":"incomplete","count":counts.tool_incomplete}]},
            "coverage":{"operationState":if complete_from == 1 {"recorded"}else{"partial"},
                "tokenTotals": if self.manifest.unapplied_retention_corrections != 0
                    || self.manifest.unapplied_retention_records != 0
                    || (range != TimeRange::all() && self.manifest.retired_observations != 0) {"partial_retention"} else {"recorded"},
                "unappliedCorrectionsAfterRetention": self.manifest.unapplied_retention_corrections,
                "unappliedObservationsAfterRetention": self.manifest.unapplied_retention_records,
                "operationStatistics": if self.manifest.unapplied_retention_records != 0 || range != TimeRange::all() && self.manifest.retired_review_operations != 0 {"partial_retention"} else {"recorded"},
                "reviewDetail": if self.manifest.review_retention_cutoff_at_ms.is_some() {"partial_retention"} else {"recorded"},
                "retiredReviewOperations": self.manifest.retired_review_operations,
                "retiredReviewContributions": self.manifest.retired_review_contributions,
                "projectionCompleteFromSequence":complete_from,"unobservedFamilies":["work_bindings"],"waits":"partial","observedWaitFamilies":["http_retry","stream_retry","compaction_retry"],"unobservedWaitFamilies":["user","agent","approval","websocket_connection","auth_refresh"]},
            "formulas":{"tokens":"Input and output are disjoint components. Cached input and reasoning output are subsets; do not add them again.",
                "counts":"Operations count canonical tool/model lifecycles, not the number of journal observations."}
        });
        crate::summary_cache::insert(
            self.root.clone(),
            self.metadata_revision,
            self.manifest.snapshot_generation,
            self.manifest.watermark,
            self.journal_watermark(),
            self.pending_entries(),
            self.manifest.pending_corrections.len(),
            range,
            filter,
            value.clone(),
        );
        Ok(value)
    }

    fn count_tail(
        &self,
        descriptor: Option<&ReviewProjectionDescriptor>,
        filter: &UsageFilter,
        mut counts: ReviewCounts,
    ) -> Result<ReviewCounts> {
        if !self.records.iter().any(|row| {
            lifecycle(row).is_some() || row.event.as_deref() == Some("classification_correction")
        }) {
            return Ok(counts);
        }
        let connection = Connection::open_in_memory().map_err(query_error)?;
        let tail = canonical_tail(self, &connection)?;
        let ids = tail
            .iter()
            .filter(|row| {
                lifecycle(row).is_some()
                    || row.event.as_deref() == Some("classification_correction")
            })
            .filter_map(|row| row.operation_id.clone())
            .collect::<BTreeSet<_>>();
        let mut states = descriptor
            .map_or_else(
                || Ok(Vec::new()),
                |descriptor| load_review_operations(&self.root, descriptor, &ids, &connection),
            )?
            .into_iter()
            .map(|row| (row.operation_id.clone(), row))
            .collect::<BTreeMap<_, _>>();
        let mut classifications = BTreeMap::<String, ReviewClassification>::new();
        if let Some(descriptor) = descriptor
            && !descriptor.classification_files.is_empty()
            && !ids.is_empty()
        {
            let source = parquet_source(&self.root, &descriptor.classification_files)?;
            let selected = ids
                .iter()
                .map(|id| quoted(id))
                .collect::<Vec<_>>()
                .join(", ");
            classifications = query_rows::<ReviewClassification>(&connection, &format!(
                "SELECT * FROM {source} WHERE operation_id IN ({selected}) QUALIFY row_number() OVER (PARTITION BY operation_id ORDER BY source_sequence DESC) = 1"
            ), ids.len())?.into_iter().map(|row| (row.operation_id.clone(), row)).collect();
        }
        for state in states
            .values()
            .filter(|state| matches_filter(state, filter))
        {
            counts = counts.checked_sub(&effective_counts(
                state,
                classifications.get(&state.operation_id),
            ))?;
        }
        for record in tail {
            let sequence = self.record_sequences[&record.id];
            if lifecycle(record).is_some() {
                let id = record.operation_id.as_deref().unwrap_or_default();
                if let Some(state) = states.get_mut(id) {
                    state.merge(sequence, record)?;
                } else {
                    let state = ReviewOperation::from_fact(sequence, record)?;
                    states.insert(state.operation_id.clone(), state);
                }
            }
            if record.event.as_deref() == Some("classification_correction")
                && let Some(id) = &record.operation_id
            {
                classifications.insert(
                    id.clone(),
                    ReviewClassification {
                        record_id: record.id.clone(),
                        operation_id: id.clone(),
                        source_sequence: sequence,
                        observed_at_ms: record.effective_observed_at_ms(),
                        phase: record.phase.clone(),
                        activity: record.activity.clone(),
                        classification: record.classification.clone(),
                    },
                );
            }
        }
        for state in states
            .values()
            .filter(|state| matches_filter(state, filter))
        {
            counts = counts.checked_add(&effective_counts(
                state,
                classifications.get(&state.operation_id),
            ))?;
        }
        Ok(counts)
    }
}

fn count_scope(filter: &UsageFilter) -> Option<ReviewCountScope> {
    if filter.model.is_some() || filter.thread_ids.is_some() {
        return None;
    }
    match (&filter.account_id, &filter.repository_id, &filter.thread_id) {
        (None, None, None) => Some(ReviewCountScope::Global),
        (Some(id), None, None) => Some(ReviewCountScope::Account(id.clone())),
        (None, Some(id), None) => Some(ReviewCountScope::Repository(id.clone())),
        (None, None, Some(id)) => Some(ReviewCountScope::Thread(id.clone())),
        _ => None,
    }
}

fn count_scope_key(scope: &ReviewCountScope) -> String {
    match scope {
        ReviewCountScope::Global => "all".to_string(),
        ReviewCountScope::Account(id) => format!("account:{id}"),
        ReviewCountScope::Repository(id) => format!("repository:{id}"),
        ReviewCountScope::Thread(id) => format!("thread:{id}"),
    }
}

fn effective_counts(
    state: &ReviewOperation,
    correction: Option<&ReviewClassification>,
) -> ReviewCounts {
    if state.kind == crate::ReviewOperationKind::Wait {
        return ReviewCounts::default();
    }
    let mut counts = ReviewCounts::for_operation(state);
    if let Some(correction) = correction {
        counts.unclassified = u64::from(
            correction.phase.as_ref().or(state.phase.as_ref()).is_none()
                || correction
                    .activity
                    .as_ref()
                    .or(state.activity.as_ref())
                    .is_none(),
        );
    }
    counts
}

fn matches_filter(state: &ReviewOperation, filter: &UsageFilter) -> bool {
    [
        &filter.account_id,
        &filter.repository_id,
        &filter.thread_id,
        &filter.model,
    ]
    .into_iter()
    .zip([
        &state.account_id,
        &state.repository_id,
        &state.thread_id,
        &state.model,
    ])
    .all(|(wanted, actual)| {
        wanted
            .as_ref()
            .is_none_or(|wanted| actual.as_ref() == Some(wanted))
    }) && filter.thread_ids.as_ref().is_none_or(|threads| {
        state
            .thread_id
            .as_ref()
            .is_some_and(|id| threads.contains(id))
    })
}
