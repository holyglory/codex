use crate::ActivityState;
use crate::CoverageState;
use crate::MeasurementProvenance;
use crate::OperationKind;
use crate::TokenCategoryPath;
use crate::UsageStore;
use crate::UsageStoreError;
use crate::detail_query_support::operation;
use crate::detail_query_support::optional_uuid;
use crate::detail_query_support::required_enum;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[path = "performance_review_query.rs"]
mod query;
#[path = "performance_review_types.rs"]
mod types;
#[path = "performance_review_work.rs"]
mod work;
pub use types::*;

impl UsageStore {
    pub async fn performance_review_packet(
        &self,
        query: PerformanceReviewQuery,
    ) -> Result<PerformanceReviewPacket, UsageStoreError> {
        let source = if crate::report_cache::is_ready(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            query::ClassificationSource::Cache
        } else {
            query::ClassificationSource::Canonical
        };
        let mut transaction = self.pool.begin().await.map_err(UsageStoreError::Database)?;
        let mut builder = query::selection(&query, &source);
        let coverage_row = builder
            .push(query::TOKEN_FACTS)
            .push(query::OPERATION_COVERAGE)
            .build()
            .fetch_one(transaction.as_mut())
            .await
            .map_err(UsageStoreError::Database)?;
        let missing_totals = number(&coverage_row, "model_requests_without_provider_total")?;

        let mut builder = query::selection(&query, &source);
        let token_rows = builder.push(query::TOKEN_FACTS).push("SELECT category_path, measurement_provenance,
            COALESCE(SUM(CASE WHEN COALESCE(conflict, 0) = 0 THEN token_count ELSE 0 END), 0) measured_tokens,
            COUNT(*) observations, SUM(unknown_count) unknown_observations,
            MAX(incomplete OR COALESCE(conflict, 0)) incomplete, COUNT(*) OVER () category_count
            FROM token_facts GROUP BY category_path, measurement_provenance
            ORDER BY category_path <> 'total_tokens', category_path, measurement_provenance LIMIT 12")
            .build().fetch_all(transaction.as_mut()).await.map_err(UsageStoreError::Database)?;
        let category_count = token_rows
            .first()
            .map(|row| number(row, "category_count"))
            .transpose()?
            .unwrap_or(0);
        let tokens = token_rows
            .iter()
            .map(|row| {
                let category = TokenCategoryPath::new(
                    row.try_get::<String, _>("category_path")
                        .map_err(UsageStoreError::Database)?,
                )
                .map_err(|_| UsageStoreError::InvalidFact)?;
                let provenance = required_enum(
                    row.try_get("measurement_provenance")
                        .map_err(UsageStoreError::Database)?,
                    MeasurementProvenance::parse,
                    MeasurementProvenance::as_str,
                )?;
                let measured_tokens = number(row, "measured_tokens")?;
                let incomplete = number(row, "incomplete")? > 0
                    || (category.as_str() == "total_tokens"
                        && provenance == "provider_reported"
                        && missing_totals > 0);
                Ok(ReviewTokens {
                    category: category.as_str().to_string(),
                    provenance,
                    measured_tokens,
                    exact_tokens: (!incomplete).then_some(measured_tokens),
                    observations: number(row, "observations")?,
                    unknown_observations: number(row, "unknown_observations")?,
                })
            })
            .collect::<Result<Vec<_>, UsageStoreError>>()?;

        let mut builder = query::selection(&query, &source);
        let rows = builder.push("SELECT operation_kind category, COUNT(*) count,
            COALESCE(SUM(interval_ms), 0) measured_interval_sum_ms, SUM(interval_ms IS NULL) unknown_intervals
            FROM effective GROUP BY operation_kind ORDER BY operation_kind")
            .build().fetch_all(transaction.as_mut()).await.map_err(UsageStoreError::Database)?;
        let operations = rows
            .iter()
            .map(|row| category(row, OperationKind::parse, OperationKind::as_str))
            .collect::<Result<Vec<_>, _>>()?;
        let mut builder = query::selection(&query, &source);
        let rows = builder
            .push(query::WAIT_CATEGORIES)
            .build()
            .fetch_all(transaction.as_mut())
            .await
            .map_err(UsageStoreError::Database)?;
        let waits = rows
            .iter()
            .map(|row| category(row, ActivityState::parse, ActivityState::as_str))
            .collect::<Result<Vec<_>, _>>()?;

        let mut builder = query::selection(&query, &source);
        let rows = builder.push("SELECT * FROM effective WHERE retry_of_operation_id IS NOT NULL OR rework_of_operation_id IS NOT NULL
            ORDER BY started_at_ms, id LIMIT 5").build().fetch_all(transaction.as_mut())
            .await.map_err(UsageStoreError::Database)?;
        let sample = rows.iter().map(link).collect::<Result<Vec<_>, _>>()?;
        let links = ReviewLinks {
            retry_operations: number(&coverage_row, "retry_operations")?,
            rework_operations: number(&coverage_row, "rework_operations")?,
            omitted_operations: number(&coverage_row, "linked_operations")?
                .saturating_sub(sample.len() as u64),
            sample,
        };
        let mut builder = query::selection(&query, &source);
        let rows = builder.push("SELECT *, COUNT(*) OVER () candidate_count FROM effective
            WHERE (retry_of_operation_id IS NOT NULL OR rework_of_operation_id IS NOT NULL)
              AND effective_state IN ('model_active', 'tool_active') AND effective_phase <> 'testing'
              AND effective_activity NOT IN ('build_validation', 'verification_review', 'unit_testing', 'integration_testing',
                'browser_qa', 'compatibility_testing', 'migration_rehearsal')
            ORDER BY interval_ms IS NULL, interval_ms DESC, started_at_ms, id LIMIT 5")
            .build().fetch_all(transaction.as_mut()).await.map_err(UsageStoreError::Database)?;
        let candidate_count = rows
            .first()
            .map(|row| number(row, "candidate_count"))
            .transpose()?
            .unwrap_or(0);
        let candidates = rows
            .iter()
            .map(|row| {
                let evidence = link(row)?;
                let signal = match (
                    &evidence.retry_of_operation_id,
                    &evidence.rework_of_operation_id,
                ) {
                    (Some(_), Some(_)) => "linked_retry_and_rework",
                    (Some(_), None) => "linked_retry",
                    (None, Some(_)) => "linked_rework",
                    (None, None) => return Err(UsageStoreError::InvalidFact),
                };
                Ok(ReviewCandidate {
                    signal,
                    measured_interval_ms: row
                        .try_get::<Option<i64>, _>("interval_ms")
                        .map_err(UsageStoreError::Database)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| UsageStoreError::DatabaseValueOutOfRange)?,
                    critical_path: "unknown",
                    avoidability: "unknown",
                    evidence,
                })
            })
            .collect::<Result<Vec<_>, UsageStoreError>>()?;
        let mut builder = query::selection(&query, &source);
        let rows = builder
            .push(
                "SELECT coverage.coverage_state, COUNT(*) count FROM coverage_events coverage
            JOIN scoped ON scoped.id = coverage.operation_id CROSS JOIN bounds
            WHERE coverage.occurred_at_ms >= lower_ms AND coverage.occurred_at_ms < upper_ms
            GROUP BY coverage.coverage_state ORDER BY coverage.coverage_state",
            )
            .build()
            .fetch_all(transaction.as_mut())
            .await
            .map_err(UsageStoreError::Database)?;
        let coverage_events = rows
            .iter()
            .map(|row| {
                Ok(ReviewCoverageCount {
                    state: required_enum(
                        row.try_get("coverage_state")
                            .map_err(UsageStoreError::Database)?,
                        CoverageState::parse,
                        CoverageState::as_str,
                    )?,
                    count: number(row, "count")?,
                })
            })
            .collect::<Result<Vec<_>, UsageStoreError>>()?;
        let work_bindings = work::read(transaction.as_mut(), &query, &source).await?;
        transaction
            .commit()
            .await
            .map_err(UsageStoreError::Database)?;
        let window = ReviewWindow {
            from_at_ms: query
                .time_range
                .map(super::report_math::UtcTimeRange::start_ms),
            to_at_ms: query
                .time_range
                .map(super::report_math::UtcTimeRange::end_ms),
        };
        let coverage = ReviewCoverage {
            raw_operations: number(&coverage_row, "raw_operations")?,
            deduplicated_operations: number(&coverage_row, "deduplicated_operations")?,
            unknown_operation_intervals: number(&coverage_row, "unknown_operation_intervals")?,
            unclassified_operations: number(&coverage_row, "unclassified_operations")?,
            operations_without_repository: number(&coverage_row, "operations_without_repository")?,
            unlinked_tool_operations: number(&coverage_row, "unlinked_tool_operations")?,
            model_requests_without_provider_total: missing_totals,
            raw_token_observations: number(&coverage_row, "raw_token_observations")?,
            deduplicated_token_observations: number(
                &coverage_row,
                "deduplicated_token_observations",
            )?,
            unknown_token_observations: number(&coverage_row, "unknown_token_observations")?,
            incomplete_token_observations: number(&coverage_row, "incomplete_token_observations")?,
            conflicting_token_observations: number(
                &coverage_row,
                "conflicting_token_observations",
            )?,
            unattributed_coverage_events_in_window: number(
                &coverage_row,
                "unattributed_coverage_events_in_window",
            )?,
            omitted_token_categories: category_count.saturating_sub(tokens.len() as u64),
            omitted_candidates: candidate_count.saturating_sub(candidates.len() as u64),
            coverage_events,
            collection_completeness: "unknown",
            critical_path: "not_collected",
            repeated_input_comparison: "not_collected",
        };
        Ok(PerformanceReviewPacket {
            schema_version: 1,
            kind: "performanceReview",
            evidence: ReviewEvidence {
                action: "details",
                details: ["operations", "tokens", "coverage", "activity_spans"],
                repository: query.repository_id.map(|id| id.as_str().to_string()),
                thread_id: query.thread_id.map(|id| id.as_str().to_string()),
                from_at_ms: window.from_at_ms,
                to_at_ms: window.to_at_ms,
                limit: 25,
            },
            window,
            tokens,
            operations,
            waits,
            links,
            candidates,
            coverage,
            work_bindings,
            interpretation: "Candidates are signals, not diagnoses or measured waste. Critical path and avoidability are unknown. Manual waits and deliberate validation are not waste. Interval sums are effort, not elapsed wall time; waits overlap operation effort. Token categories and provenances overlap: never add total, input, cached, output, or reasoning categories together. Facts deduplicate by owner, source event, category and provenance; covered tools retain request ownership. Windows clip intervals and select facts by observation time. Evidence IDs reference operations; select one details family and follow its nextCursor. Omitted groups remain in paginated evidence. Collection gaps may be unobservable; zero recorded gaps does not prove complete capture.",
        })
    }
}

fn number(row: &SqliteRow, name: &str) -> Result<u64, UsageStoreError> {
    row.try_get::<i64, _>(name)
        .map_err(UsageStoreError::Database)?
        .try_into()
        .map_err(|_| UsageStoreError::DatabaseValueOutOfRange)
}

fn category<Category: Copy>(
    row: &SqliteRow,
    parse: impl Fn(&str) -> Option<Category>,
    display: impl Fn(Category) -> &'static str,
) -> Result<ReviewCategory, UsageStoreError> {
    Ok(ReviewCategory {
        category: required_enum(
            row.try_get("category").map_err(UsageStoreError::Database)?,
            parse,
            display,
        )?,
        count: number(row, "count")?,
        measured_interval_sum_ms: number(row, "measured_interval_sum_ms")?,
        unknown_intervals: number(row, "unknown_intervals")?,
    })
}

fn link(row: &SqliteRow) -> Result<ReviewLink, UsageStoreError> {
    Ok(ReviewLink {
        operation_id: operation(row.try_get("id").map_err(UsageStoreError::Database)?)?,
        started_at_ms: row
            .try_get("started_at_ms")
            .map_err(UsageStoreError::Database)?,
        retry_of_operation_id: optional_uuid(
            row.try_get("retry_of_operation_id")
                .map_err(UsageStoreError::Database)?,
        )?,
        rework_of_operation_id: optional_uuid(
            row.try_get("rework_of_operation_id")
                .map_err(UsageStoreError::Database)?,
        )?,
    })
}

#[cfg(test)]
#[path = "performance_review_tests.rs"]
mod tests;
