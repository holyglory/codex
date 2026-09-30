use crate::ClassificationCount;
use crate::CoverageCount;
use crate::CoverageSummary;
use crate::ParticipationCounts;
use crate::TokenActivityAggregate;
use crate::TokenAggregate;
use crate::UsageStore;
use crate::UsageStoreError;
use crate::UsageSummary;
use crate::UsageSummaryQuery;
use crate::UsageSummaryScope;
use crate::types::TAXONOMY_VERSION;
use sqlx::Row;
use sqlx::SqliteConnection;

#[path = "report_cost_query.rs"]
mod cost;
#[path = "report_sql_selection.rs"]
mod selection;
#[path = "report_sql_time.rs"]
mod time;

const MAX_GROUPS: usize = 16_384;

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReportSource {
    CachedAll,
    CachedScoped,
    Canonical,
}

impl UsageStore {
    pub(crate) async fn bounded_usage_summary(
        &self,
        mut query: UsageSummaryQuery,
    ) -> Result<UsageSummary, UsageStoreError> {
        query.repository_id = match query.repository_id {
            Some(id) => Some(self.canonical_repository_id(&id).await?),
            None => None,
        };
        let family = match &query.repository_id {
            Some(id) => Some(self.repository_family_ids(id).await?),
            None => None,
        };
        let scope = match (&query.thread_id, &query.repository_id) {
            (Some(id), _) => UsageSummaryScope::Thread(id.clone()),
            (None, Some(id)) => UsageSummaryScope::Repository(id.clone()),
            (None, None) => UsageSummaryScope::All,
        };
        let include_global = matches!(scope, UsageSummaryScope::All)
            && query.account_profile_ref.is_none()
            && family.is_none();
        let _reader = self
            .report_refresh
            .reader
            .acquire()
            .await
            .map_err(|_| database_error(sqlx::Error::PoolClosed))?;
        let mut connection = self.pool.acquire().await.map_err(database_error)?;
        // Five store connections at 8 MiB each leave room for bounded returned
        // groups under the 128 MiB accounting working-memory target. Sorts and
        // selections spill to SQLite-owned temporary files instead of Rust Vecs.
        sqlx::raw_sql(
            "PRAGMA temp_store = FILE; PRAGMA cache_size = -8192; PRAGMA temp.cache_size = -2048;",
        )
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
        use sqlx::Connection;
        let mut tx = connection.begin().await.map_err(database_error)?;
        let source = if crate::report_cache::is_ready_on(&mut tx)
            .await
            .unwrap_or(false)
        {
            if include_global && query.time_range.is_none() {
                ReportSource::CachedAll
            } else {
                ReportSource::CachedScoped
            }
        } else {
            self.report_refresh.kick(self.pool.clone());
            ReportSource::Canonical
        };
        selection::select(&mut tx, &query, family.as_ref(), source)
            .await
            .map_err(database_error)?;
        let tokens = token_aggregates(&mut tx, source).await?;
        let provider_tokens_by_activity = activity_aggregates(&mut tx, source).await?;
        let token_counts = rows(&mut tx, match source {
            ReportSource::CachedAll => "SELECT coverage_state AS state, observation_count AS n FROM _usage_report_token_coverage ORDER BY coverage_state",
            ReportSource::CachedScoped | ReportSource::Canonical => "SELECT coverage_state AS state, SUM(observation_count) AS n FROM _usage_tokens GROUP BY coverage_state ORDER BY coverage_state",
        }).await?;
        let token_observation_counts = counts(token_counts)?;
        let event_rows = if source == ReportSource::CachedAll {
            rows(&mut tx, "SELECT coverage_state AS state, observation_count AS n FROM _usage_report_coverage ORDER BY coverage_state").await?
        } else {
            sqlx::query(r#"
            SELECT coverage_state AS state, COUNT(*) AS n FROM coverage_events
            WHERE (operation_id IN (SELECT id FROM _usage_selected) OR (?1 AND operation_id IS NULL))
              AND (?2 IS NULL OR occurred_at_ms >= ?2) AND (?3 IS NULL OR occurred_at_ms < ?3)
            GROUP BY coverage_state ORDER BY coverage_state
        "#).bind(include_global).bind(query.time_range.map(crate::UtcTimeRange::start_ms)).bind(query.time_range.map(crate::UtcTimeRange::end_ms))
            .fetch_all(&mut *tx).await.map_err(database_error)?
        };
        let event_counts = counts(event_rows)?;
        let unresolved: bool = if source != ReportSource::Canonical && query.time_range.is_none() {
            sqlx::query_scalar(r#"
                SELECT EXISTS(SELECT 1 FROM _usage_report_latest_coverage AS coverage
                    JOIN _usage_selected AS selected ON selected.id = coverage.operation_id
                    WHERE coverage.coverage_state <> 'complete' AND NOT (
                        coverage.scope_kind IN ('model_attempt','tool_attempt') AND coverage.coverage_state IN ('capture_started','partial')
                        AND coverage.reason_code IS NULL AND selected.terminal_status IS NOT NULL
                        AND (coverage.scope_kind = 'tool_attempt' OR EXISTS(SELECT 1 FROM _usage_report_provider_complete AS provider WHERE provider.operation_id = coverage.operation_id))
                    )) OR (? AND EXISTS(SELECT 1 FROM _usage_report_global_gap WHERE has_gap = 1))
            "#).bind(include_global).fetch_one(&mut *tx).await.map_err(database_error)?
        } else {
            sqlx::query_scalar(r#"
            SELECT EXISTS(SELECT 1 FROM coverage_events AS coverage
            WHERE (operation_id IN (SELECT id FROM _usage_selected) OR (?1 AND operation_id IS NULL))
              AND coverage_state <> 'complete'
              AND (?2 IS NULL OR occurred_at_ms >= ?2) AND (?3 IS NULL OR occurred_at_ms < ?3)
              AND NOT (
                scope_kind IN ('model_attempt','tool_attempt') AND coverage_state IN ('capture_started','partial') AND reason_code IS NULL
                AND EXISTS (SELECT 1 FROM operation_events AS terminal WHERE terminal.operation_id = coverage.operation_id AND terminal.terminal = 1)
                AND (scope_kind = 'tool_attempt' OR EXISTS (
                    SELECT 1 FROM model_requests AS request JOIN token_observations AS token ON token.model_request_id = request.id
                    WHERE request.operation_id = coverage.operation_id AND token.measurement_provenance = 'provider_reported'
                      AND token.token_count IS NOT NULL AND token.coverage_state = 'complete' AND token.category_path NOT GLOB 'attribution.items.*'
                      AND (?2 IS NULL OR token.observed_at_ms >= ?2) AND (?3 IS NULL OR token.observed_at_ms < ?3)
                ))
              ) AND NOT EXISTS (
                SELECT 1 FROM coverage_events AS later WHERE later.operation_id = coverage.operation_id AND later.scope_kind = coverage.scope_kind
                  AND (later.occurred_at_ms > coverage.occurred_at_ms OR (later.occurred_at_ms = coverage.occurred_at_ms AND later.event_id > coverage.event_id))
                  AND (?3 IS NULL OR later.occurred_at_ms < ?3)
              ))
        "#).bind(include_global).bind(query.time_range.map(crate::UtcTimeRange::start_ms)).bind(query.time_range.map(crate::UtcTimeRange::end_ms))
            .fetch_one(&mut *tx).await.map_err(database_error)?
        };
        let row = sqlx::query("SELECT COUNT(*) AS n, COALESCE(SUM(operation_kind = 'model_request'),0) AS models, COALESCE(SUM(terminal_status IS NULL),0) AS unfinished FROM _usage_selected")
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        let operation_count = count(row.get("n"))?;
        let model_request_count = count(row.get("models"))?;
        let unfinished_operations = count(row.get("unfinished"))?;
        let classifications = rows(&mut tx, "SELECT phase, activity, provenance, COUNT(*) AS n FROM _usage_selected GROUP BY phase, activity, provenance ORDER BY phase, activity, provenance").await?
            .into_iter().map(|row| Ok(ClassificationCount {phase:row.get("phase"),activity:row.get("activity"),provenance:row.get("provenance"),count:count(row.get("n"))?})).collect::<Result<Vec<_>,UsageStoreError>>()?;
        let cost = Some(cost::cost(&mut tx, &query, family.as_ref(), source).await?);
        let (timing, tools) = time::metrics(&mut tx, query.time_range).await?;
        let has_evidence = !tokens.is_empty() || !event_counts.is_empty();
        let has_gaps = !has_evidence
            || tokens.iter().any(|t| t.exact_tokens.is_none())
            || unfinished_operations > 0
            || unresolved
            || token_observation_counts
                .iter()
                .any(|c| c.state != "complete");
        let overall_state = if !has_evidence {
            "unobserved"
        } else if has_gaps {
            "unknown"
        } else {
            "complete"
        }
        .to_string();
        let database_schema_version = count(
            sqlx::query_scalar("SELECT COALESCE(MAX(version),0) FROM _sqlx_migrations")
                .fetch_one(&mut *tx)
                .await
                .map_err(database_error)?,
        )?;
        tx.commit().await.map_err(database_error)?;
        sqlx::query("PRAGMA temp_store = FILE")
            .execute(&mut *connection)
            .await
            .map_err(database_error)?;
        Ok(UsageSummary {
            cost,
            database_schema_version,
            taxonomy_version: TAXONOMY_VERSION,
            scope,
            time_range: query.time_range,
            coverage: CoverageSummary {
                overall_state,
                event_counts,
                token_observation_counts,
                has_gaps,
                unfinished_operations,
            },
            tokens,
            provider_tokens_by_activity,
            timing,
            repository_participation: ParticipationCounts {
                operation_count,
                tool_count: tools.count,
                additive: false,
                label: "repository participation counts are non-additive; token buckets are additive",
            },
            operation_count,
            model_request_count,
            tool_count: tools.count,
            tools,
            classifications,
            aggregation: "checked sum of each stored observation once; merged repository source history resolves to its canonical target",
        })
    }
}

async fn token_aggregates(
    connection: &mut SqliteConnection,
    source: ReportSource,
) -> Result<Vec<TokenAggregate>, UsageStoreError> {
    rows(connection, match source {
        ReportSource::CachedAll => "SELECT category_path,repository_bucket,measurement_provenance,measured_tokens AS measured,unknown_observations AS unknowns,observation_count AS n,has_gap AS gap,aggregate_overflow AS overflow FROM _usage_report_token_aggregates ORDER BY category_path,repository_bucket,measurement_provenance",
        ReportSource::CachedScoped | ReportSource::Canonical => "SELECT category_path,repository_bucket,measurement_provenance,SUM(measured_tokens) AS measured,SUM(unknown_observations) AS unknowns,SUM(observation_count) AS n,MAX(coverage_state <> 'complete') AS gap,MAX(aggregate_overflow) AS overflow FROM _usage_tokens GROUP BY category_path,repository_bucket,measurement_provenance ORDER BY category_path,repository_bucket,measurement_provenance" }).await?
        .into_iter().map(|row| {
            if row.get::<i64,_>("overflow") != 0 { return Err(UsageStoreError::AggregateOverflow); }
            let measured_tokens = row.try_get("measured").map_err(database_error)?;
            let unknown_observations = count(row.get("unknowns"))?;
            Ok(TokenAggregate {category_path:row.get("category_path"),repository_bucket:row.get("repository_bucket"),measurement_provenance:row.get("measurement_provenance"),measured_tokens,
                exact_tokens:(unknown_observations == 0 && row.get::<i64,_>("gap") == 0).then_some(measured_tokens),unknown_observations,observation_count:count(row.get("n"))?})
        }).collect()
}

async fn activity_aggregates(
    connection: &mut SqliteConnection,
    source: ReportSource,
) -> Result<Vec<TokenActivityAggregate>, UsageStoreError> {
    rows(connection, match source {
        ReportSource::CachedAll => "SELECT phase,activity,provenance,SUM(measured_tokens) AS measured,SUM(unknown_observations) AS unknowns,MAX(coverage_state <> 'complete') AS gap,MAX(aggregate_overflow) AS overflow FROM _usage_report_dimension_tokens WHERE category_path = 'total_tokens' AND measurement_provenance = 'provider_reported' GROUP BY phase,activity,provenance ORDER BY phase,activity,provenance",
        ReportSource::CachedScoped | ReportSource::Canonical => "SELECT phase,activity,provenance,SUM(measured_tokens) AS measured,SUM(unknown_observations) AS unknowns,MAX(coverage_state <> 'complete') AS gap,MAX(aggregate_overflow) AS overflow FROM _usage_tokens WHERE category_path = 'total_tokens' AND measurement_provenance = 'provider_reported' GROUP BY phase,activity,provenance ORDER BY phase,activity,provenance" }).await?
        .into_iter().map(|row| {
            if row.get::<i64,_>("overflow") != 0 { return Err(UsageStoreError::AggregateOverflow); }
            let measured_tokens = row.try_get("measured").map_err(database_error)?;
            let unknown_observations = count(row.get("unknowns"))?;
            Ok(TokenActivityAggregate {phase:row.get("phase"),activity:row.get("activity"),attribution_provenance:row.get("provenance"),measured_tokens,
                exact_tokens:(unknown_observations == 0 && row.get::<i64,_>("gap") == 0).then_some(measured_tokens),unknown_observations})
        }).collect()
}

async fn rows(
    connection: &mut SqliteConnection,
    query: &'static str,
) -> Result<Vec<sqlx::sqlite::SqliteRow>, UsageStoreError> {
    let limit = MAX_GROUPS + 1;
    let rows = sqlx::QueryBuilder::<sqlx::Sqlite>::new(query)
        .push(" LIMIT ")
        .push_bind(limit as i64)
        .build()
        .fetch_all(connection)
        .await
        .map_err(database_error)?;
    if rows.len() > MAX_GROUPS {
        return Err(UsageStoreError::ReportTooLarge);
    }
    Ok(rows)
}

fn counts(rows: Vec<sqlx::sqlite::SqliteRow>) -> Result<Vec<CoverageCount>, UsageStoreError> {
    rows.into_iter()
        .map(|row| {
            Ok(CoverageCount {
                state: row.get("state"),
                count: count(row.get("n"))?,
            })
        })
        .collect()
}

fn count(value: i64) -> Result<u64, UsageStoreError> {
    u64::try_from(value).map_err(|_| UsageStoreError::AggregateOverflow)
}

fn database_error(error: sqlx::Error) -> UsageStoreError {
    if error
        .as_database_error()
        .is_some_and(|e| e.message() == "integer overflow")
    {
        UsageStoreError::AggregateOverflow
    } else {
        UsageStoreError::Database(error)
    }
}
