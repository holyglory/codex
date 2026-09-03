use crate::report_math::ParticipationCounts;
use crate::report_math::ReportTimeMetrics;
use crate::report_math::TokenActivityAggregate;
use crate::report_math::ToolMetrics;
use crate::report_math::UtcTimeRange;
use crate::repository::RepositoryId;
use crate::store::UsageStore;
use crate::store::UsageStoreError;
use crate::types::AccountProfileRef;
use crate::types::TAXONOMY_VERSION;
use crate::types::ThreadId;
use sqlx::Row;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageSummaryScope {
    All,
    Thread(ThreadId),
    Repository(RepositoryId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSummaryQuery {
    pub thread_id: Option<ThreadId>,
    pub repository_id: Option<RepositoryId>,
    pub account_profile_ref: Option<AccountProfileRef>,
    pub time_range: Option<UtcTimeRange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageCount {
    pub state: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageSummary {
    pub overall_state: String,
    pub event_counts: Vec<CoverageCount>,
    pub token_observation_counts: Vec<CoverageCount>,
    pub has_gaps: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenAggregate {
    pub category_path: String,
    pub repository_bucket: String,
    pub measurement_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
    pub observation_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassificationCount {
    pub phase: String,
    pub activity: String,
    pub provenance: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSummary {
    pub database_schema_version: u64,
    pub taxonomy_version: i64,
    pub scope: UsageSummaryScope,
    pub time_range: Option<UtcTimeRange>,
    pub coverage: CoverageSummary,
    pub tokens: Vec<TokenAggregate>,
    pub provider_tokens_by_activity: Vec<TokenActivityAggregate>,
    pub timing: ReportTimeMetrics,
    pub tools: ToolMetrics,
    pub repository_participation: ParticipationCounts,
    pub operation_count: u64,
    pub model_request_count: u64,
    pub tool_count: u64,
    pub classifications: Vec<ClassificationCount>,
    pub aggregation: &'static str,
}

#[derive(Default)]
struct TokenAccumulator {
    measured_tokens: i64,
    unknown_observations: u64,
    observation_count: u64,
    has_gap: bool,
}

impl UsageStore {
    pub async fn usage_summary(
        &self,
        scope: UsageSummaryScope,
    ) -> Result<UsageSummary, UsageStoreError> {
        self.usage_summary_in_range(scope, /*time_range*/ None)
            .await
    }

    pub async fn usage_summary_in_range(
        &self,
        scope: UsageSummaryScope,
        time_range: Option<UtcTimeRange>,
    ) -> Result<UsageSummary, UsageStoreError> {
        let query = match scope {
            UsageSummaryScope::All => UsageSummaryQuery {
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
                time_range,
            },
            UsageSummaryScope::Thread(thread_id) => UsageSummaryQuery {
                thread_id: Some(thread_id),
                repository_id: None,
                account_profile_ref: None,
                time_range,
            },
            UsageSummaryScope::Repository(repository_id) => UsageSummaryQuery {
                thread_id: None,
                repository_id: Some(repository_id),
                account_profile_ref: None,
                time_range,
            },
        };
        self.usage_summary_query(query).await
    }

    pub async fn usage_summary_query(
        &self,
        query: UsageSummaryQuery,
    ) -> Result<UsageSummary, UsageStoreError> {
        let repository_id = match query.repository_id {
            Some(repository_id) => Some(self.canonical_repository_id(&repository_id).await?),
            None => None,
        };
        let repository_family = match &repository_id {
            Some(repository_id) => Some(self.repository_family_ids(repository_id).await?),
            None => None,
        };
        let scope = match (&query.thread_id, repository_id) {
            (Some(thread_id), _) => UsageSummaryScope::Thread(thread_id.clone()),
            (None, Some(repository_id)) => UsageSummaryScope::Repository(repository_id),
            (None, None) => UsageSummaryScope::All,
        };
        let include_global_coverage = matches!(scope, UsageSummaryScope::All)
            && query.account_profile_ref.is_none()
            && repository_family.is_none();
        let cache_eligible = matches!(&scope, UsageSummaryScope::All)
            && query.time_range.is_none()
            && query.account_profile_ref.is_none()
            && repository_family.is_none()
            && crate::report_cache::is_ready(&self.pool)
                .await
                .unwrap_or(false);
        let selection = if cache_eligible {
            self.build_cached_all_report_selection().await?
        } else {
            self.build_report_selection(
                scope,
                query.time_range,
                repository_family.as_ref(),
                query.account_profile_ref.as_ref(),
            )
            .await?
        };
        let scope = selection.scope.clone();
        let operation_selected = |operation_id: &str| selection.contains_operation(operation_id);

        let mut token_groups = BTreeMap::<(String, String, String), TokenAccumulator>::new();
        let mut activity_groups = BTreeMap::<(String, String, String), TokenAccumulator>::new();
        let mut token_coverage = BTreeMap::<String, u64>::new();
        let mut has_token_coverage_gap = false;
        if selection.uses_report_cache() {
            for row in sqlx::query(
                r#"
                SELECT category_path, repository_bucket, measurement_provenance,
                       measured_tokens, unknown_observations, observation_count,
                       has_gap, aggregate_overflow
                FROM _usage_report_token_aggregates
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
            {
                if row.get::<i64, _>("aggregate_overflow") != 0 {
                    return Err(UsageStoreError::AggregateOverflow);
                }
                let has_gap = row.get::<i64, _>("has_gap") != 0;
                has_token_coverage_gap |= has_gap;
                token_groups.insert(
                    (
                        row.get("category_path"),
                        row.get("repository_bucket"),
                        row.get("measurement_provenance"),
                    ),
                    TokenAccumulator {
                        measured_tokens: row.get("measured_tokens"),
                        unknown_observations: u64::try_from(
                            row.get::<i64, _>("unknown_observations"),
                        )
                        .map_err(|_| UsageStoreError::AggregateOverflow)?,
                        observation_count: u64::try_from(row.get::<i64, _>("observation_count"))
                            .map_err(|_| UsageStoreError::AggregateOverflow)?,
                        has_gap,
                    },
                );
            }
            for row in sqlx::query(
                "SELECT coverage_state, observation_count FROM _usage_report_token_coverage",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
            {
                let coverage_state: String = row.get("coverage_state");
                let count = u64::try_from(row.get::<i64, _>("observation_count"))
                    .map_err(|_| UsageStoreError::AggregateOverflow)?;
                has_token_coverage_gap |= coverage_state != "complete";
                token_coverage.insert(coverage_state, count);
            }
            for row in sqlx::query(
                r#"
                SELECT operation_id, measured_tokens, unknown_observations,
                       has_gap, aggregate_overflow
                FROM _usage_report_activity_tokens
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
            {
                if row.get::<i64, _>("aggregate_overflow") != 0 {
                    return Err(UsageStoreError::AggregateOverflow);
                }
                let operation_id: String = row.get("operation_id");
                let Some((phase, activity, provenance)) = selection.classification(&operation_id)
                else {
                    continue;
                };
                let aggregate = activity_groups
                    .entry((
                        phase.to_string(),
                        activity.to_string(),
                        provenance.to_string(),
                    ))
                    .or_default();
                aggregate.measured_tokens = aggregate
                    .measured_tokens
                    .checked_add(row.get("measured_tokens"))
                    .ok_or(UsageStoreError::AggregateOverflow)?;
                aggregate.unknown_observations = aggregate
                    .unknown_observations
                    .checked_add(
                        u64::try_from(row.get::<i64, _>("unknown_observations"))
                            .map_err(|_| UsageStoreError::AggregateOverflow)?,
                    )
                    .ok_or(UsageStoreError::AggregateOverflow)?;
                aggregate.has_gap |= row.get::<i64, _>("has_gap") != 0;
            }
        } else {
            let token_rows = sqlx::query(
                r#"
                SELECT token.category_path, token.repository_bucket,
                       token.measurement_provenance, token.token_count,
                       token.coverage_state, token.observed_at_ms,
                       COALESCE(request.operation_id, tool.operation_id) AS operation_id
                FROM token_observations AS token
                LEFT JOIN model_requests AS request ON request.id = token.model_request_id
                LEFT JOIN tool_invocations AS tool ON tool.id = token.tool_invocation_id
                WHERE token.category_path NOT GLOB 'attribution.items.*'
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
            for row in token_rows {
                let operation_id: String = row.get("operation_id");
                let repository_bucket: String = row.get("repository_bucket");
                if !operation_selected(&operation_id)
                    || !selection.contains_timestamp(row.get("observed_at_ms"))
                    || repository_family
                        .as_ref()
                        .is_some_and(|family| !family.contains(&repository_bucket))
                {
                    continue;
                }
                let coverage_state: String = row.get("coverage_state");
                has_token_coverage_gap |= coverage_state != "complete";
                increment_count(token_coverage.entry(coverage_state.clone()).or_default())?;
                let category_path: String = row.get("category_path");
                let measurement_provenance: String = row.get("measurement_provenance");
                // Activity shares use the provider's additive request total. Input, output, cached,
                // and reasoning categories remain independently visible above but are subsets of it.
                let provider_total = category_path == "total_tokens"
                    && measurement_provenance == "provider_reported";
                let key = (category_path, repository_bucket, measurement_provenance);
                let accumulator = token_groups.entry(key).or_default();
                increment_count(&mut accumulator.observation_count)?;
                accumulator.has_gap |= coverage_state != "complete";
                match row.get::<Option<i64>, _>("token_count") {
                    Some(count) => {
                        accumulator.measured_tokens = accumulator
                            .measured_tokens
                            .checked_add(count)
                            .ok_or(UsageStoreError::AggregateOverflow)?;
                    }
                    None => increment_count(&mut accumulator.unknown_observations)?,
                }
                if provider_total
                    && let Some((phase, activity, provenance)) =
                        selection.classification(&operation_id)
                {
                    let aggregate = activity_groups
                        .entry((
                            phase.to_string(),
                            activity.to_string(),
                            provenance.to_string(),
                        ))
                        .or_default();
                    increment_count(&mut aggregate.observation_count)?;
                    aggregate.has_gap |= coverage_state != "complete";
                    match row.get::<Option<i64>, _>("token_count") {
                        Some(count) => {
                            aggregate.measured_tokens = aggregate
                                .measured_tokens
                                .checked_add(count)
                                .ok_or(UsageStoreError::AggregateOverflow)?;
                        }
                        None => increment_count(&mut aggregate.unknown_observations)?,
                    }
                }
            }
        }
        let tokens = token_groups
            .into_iter()
            .map(
                |((category_path, repository_bucket, measurement_provenance), aggregate)| {
                    TokenAggregate {
                        category_path,
                        repository_bucket,
                        measurement_provenance,
                        measured_tokens: aggregate.measured_tokens,
                        exact_tokens: (aggregate.unknown_observations == 0 && !aggregate.has_gap)
                            .then_some(aggregate.measured_tokens),
                        unknown_observations: aggregate.unknown_observations,
                        observation_count: aggregate.observation_count,
                    }
                },
            )
            .collect::<Vec<_>>();
        let provider_tokens_by_activity = activity_groups
            .into_iter()
            .map(
                |((phase, activity, attribution_provenance), aggregate)| TokenActivityAggregate {
                    phase,
                    activity,
                    attribution_provenance,
                    measured_tokens: aggregate.measured_tokens,
                    exact_tokens: (aggregate.unknown_observations == 0 && !aggregate.has_gap)
                        .then_some(aggregate.measured_tokens),
                    unknown_observations: aggregate.unknown_observations,
                },
            )
            .collect();

        let mut coverage_events = BTreeMap::<String, u64>::new();
        if selection.uses_report_cache() {
            for row in sqlx::query(
                r#"
                SELECT coverage_state, observation_count
                FROM _usage_report_coverage
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
            {
                let count = u64::try_from(row.get::<i64, _>("observation_count"))
                    .map_err(|_| UsageStoreError::AggregateOverflow)?;
                let aggregate = coverage_events
                    .entry(row.get("coverage_state"))
                    .or_default();
                *aggregate = aggregate
                    .checked_add(count)
                    .ok_or(UsageStoreError::AggregateOverflow)?;
            }
        } else {
            for row in sqlx::query(
                "SELECT operation_id, coverage_state, occurred_at_ms FROM coverage_events",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
            {
                let operation_id = row.get::<Option<String>, _>("operation_id");
                let selected = match operation_id.as_deref() {
                    Some(operation_id) => operation_selected(operation_id),
                    None => include_global_coverage,
                } && selection.contains_timestamp(row.get("occurred_at_ms"));
                if selected {
                    increment_count(
                        coverage_events
                            .entry(row.get("coverage_state"))
                            .or_default(),
                    )?;
                }
            }
        }
        let has_evidence = !tokens.is_empty() || !coverage_events.is_empty();
        let has_gaps = !has_evidence
            || tokens.iter().any(|token| token.exact_tokens.is_none())
            || coverage_events.keys().any(|state| state != "complete")
            || has_token_coverage_gap;
        let overall_state = if !has_evidence {
            "unobserved"
        } else if has_gaps {
            "unknown"
        } else {
            "complete"
        };

        let operation_count = u64::try_from(selection.operations.len())
            .map_err(|_| UsageStoreError::AggregateOverflow)?;
        let model_request_count = u64::try_from(
            selection
                .operations
                .iter()
                .filter(|operation| operation.kind == "model_request")
                .count(),
        )
        .map_err(|_| UsageStoreError::AggregateOverflow)?;
        let (timing, tools) = self.report_time_metrics(&selection).await?;
        let tool_count = tools.count;

        let mut classification_groups = BTreeMap::<(String, String, String), u64>::new();
        for operation in &selection.operations {
            increment_count(
                classification_groups
                    .entry((
                        operation.phase.clone(),
                        operation.activity.clone(),
                        operation.attribution_provenance.clone(),
                    ))
                    .or_default(),
            )?;
        }
        let classifications = classification_groups
            .into_iter()
            .map(
                |((phase, activity, provenance), count)| ClassificationCount {
                    phase,
                    activity,
                    provenance,
                    count,
                },
            )
            .collect();

        let database_schema_version =
            sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                .fetch_one(&self.pool)
                .await
                .map_err(UsageStoreError::Database)?
                .try_into()
                .map_err(|_| UsageStoreError::DatabaseValueOutOfRange)?;

        Ok(UsageSummary {
            database_schema_version,
            taxonomy_version: TAXONOMY_VERSION,
            scope,
            time_range: query.time_range,
            coverage: CoverageSummary {
                overall_state: overall_state.to_string(),
                event_counts: coverage_counts(coverage_events),
                token_observation_counts: coverage_counts(token_coverage),
                has_gaps,
            },
            tokens,
            provider_tokens_by_activity,
            timing,
            tools,
            repository_participation: ParticipationCounts {
                operation_count,
                tool_count,
                additive: false,
                label: "repository participation counts are non-additive; token buckets are additive",
            },
            operation_count,
            model_request_count,
            tool_count,
            classifications,
            aggregation: "checked sum of each stored observation once; merged repository source history resolves to its canonical target",
        })
    }
}

fn coverage_counts(counts: BTreeMap<String, u64>) -> Vec<CoverageCount> {
    counts
        .into_iter()
        .map(|(state, count)| CoverageCount { state, count })
        .collect()
}

fn increment_count(count: &mut u64) -> Result<(), UsageStoreError> {
    *count = count
        .checked_add(1)
        .ok_or(UsageStoreError::AggregateOverflow)?;
    Ok(())
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
