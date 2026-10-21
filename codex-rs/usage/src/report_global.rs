//! Incremental global-report projections.
//!
//! The core report cache serves finite and scoped reads. This component keeps
//! all-scope counters and dirty hour identities bounded so a global read never
//! scans the raw collector history. Pages are idempotent: the page transaction
//! updates the source cursor and these projections together.
use crate::DurationAggregate;
use crate::NamedDuration;
use crate::ReportTimeMetrics;
use crate::ToolMetrics;
use crate::ToolOutcomeCount;
use crate::UsageStoreError;
use sqlx::Connection;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use tokio::time::Duration;
use tokio::time::Instant;

#[path = "report_global_schema.rs"]
mod schema;

pub(crate) use schema::COMPONENT_VERSION;
pub(crate) use schema::PREVIOUS_COMPONENT_VERSION;
use schema::RESET_SQL;
use schema::SCHEMA;
use schema::SEED_SCHEMA;

const SEED_SOURCES: [&str; 4] = [
    "operations",
    "token_observations",
    "coverage_events",
    "activity_spans",
];

pub(crate) async fn initialize(
    connection: &mut SqliteConnection,
    core_schema_version: i64,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(SCHEMA).execute(&mut *connection).await?;
    initialize_seed_frontier(connection, /*complete*/ false).await?;
    sqlx::query(
        "UPDATE _usage_report_global_meta SET component_version=?, core_schema_version=? WHERE singleton=1",
    )
        .bind(COMPONENT_VERSION)
        .bind(core_schema_version)
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "UPDATE _usage_report_global_meta SET ready=0, dirty_revision=dirty_revision+1 WHERE singleton=1",
    )
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn reset(
    connection: &mut SqliteConnection,
    core_schema_version: i64,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(RESET_SQL).execute(&mut *connection).await?;
    initialize(connection, core_schema_version).await
}

/// Upgrade the v2 component in place. V2 populated every derived projection
/// in one transaction, so its source history is already represented. The v3
/// seed frontier therefore starts at each source high-water mark and does not
/// invalidate the queues, aggregates, or readiness state that v2 left behind.
pub(crate) async fn upgrade_v2(
    connection: &mut SqliteConnection,
    core_schema_version: i64,
) -> Result<(), sqlx::Error> {
    initialize_seed_frontier(connection, /*complete*/ true).await?;
    sqlx::query(
        "UPDATE _usage_report_global_meta SET component_version=?, core_schema_version=? WHERE singleton=1",
    )
    .bind(COMPONENT_VERSION)
    .bind(core_schema_version)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn initialize_seed_frontier(
    connection: &mut SqliteConnection,
    complete: bool,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(SEED_SCHEMA).execute(&mut *connection).await?;
    sqlx::query("DELETE FROM _usage_report_global_seed")
        .execute(&mut *connection)
        .await?;
    for source in SEED_SOURCES {
        let mut query =
            sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT COALESCE(MAX(rowid),0) FROM ");
        query.push(source);
        let high_water = query
            .build_query_scalar()
            .fetch_one(&mut *connection)
            .await?;
        sqlx::query(
            "INSERT INTO _usage_report_global_seed(source,cursor,high_water) VALUES (?, ?, ?)",
        )
        .bind(source)
        .bind(if complete { high_water } else { 0 })
        .bind(high_water)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

pub(crate) async fn is_supported(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    let component: Option<i64> = sqlx::query_scalar(
        "SELECT component_version FROM _usage_report_global_meta WHERE singleton=1",
    )
    .fetch_optional(&mut *connection)
    .await?;
    Ok(component.is_none_or(|version| version <= COMPONENT_VERSION))
}

pub(crate) async fn is_ready_on(connection: &mut SqliteConnection) -> Result<bool, sqlx::Error> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='_usage_report_global_meta')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if exists == 0 {
        return Ok(false);
    }
    let seed_exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='_usage_report_global_seed')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if seed_exists == 0 {
        return Ok(false);
    }
    let state: Option<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT component_version, core_schema_version, core_generation, ready, dirty_revision, applied_revision FROM _usage_report_global_meta WHERE singleton=1",
    )
    .fetch_optional(&mut *connection)
    .await?;
    let generation: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(high_water),0) FROM _usage_report_backfill")
            .fetch_one(&mut *connection)
            .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM _usage_report_global_seed WHERE cursor < high_water) OR EXISTS(SELECT 1 FROM _usage_report_global_dirty_operations) OR EXISTS(SELECT 1 FROM _usage_report_global_dirty_hours) OR EXISTS(SELECT 1 FROM _usage_report_global_dirty_hour_ranges)",
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(
        state.is_some_and(|(version, core, core_generation, ready, dirty, applied)| {
            version == COMPONENT_VERSION
                && core == crate::report_cache::REPORT_CACHE_SCHEMA_VERSION
                && core_generation == generation
                && ready == 1
                && dirty == applied
                && pending == 0
        }),
    )
}

/// Read the all-scope timing projection after every dirty hour has been rebuilt.
///
/// Each hour stores intervals clipped to that half-open hour. Summing the
/// disjoint hour parts therefore preserves the canonical union while keeping
/// the read independent of the raw operation and span history.
pub(crate) async fn time_metrics(
    connection: &mut SqliteConnection,
) -> Result<(ReportTimeMetrics, ToolMetrics), UsageStoreError> {
    let rows = sqlx::query(
        "SELECT kind,label,COALESCE(SUM(measured_ms),0) AS measured_ms,COALESCE(SUM(unknown_intervals),0) AS unknown_intervals,COALESCE(SUM(zero_intervals),0) AS zero_intervals FROM _usage_report_global_time GROUP BY kind,label ORDER BY kind,label",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(UsageStoreError::Database)?;
    let mut timing = ReportTimeMetrics {
        execution_wall_union: duration(/*milliseconds*/ 0, /*unknown*/ 0)?,
        summed_per_agent_active: duration(/*milliseconds*/ 0, /*unknown*/ 0)?,
        ..ReportTimeMetrics::default()
    };
    for row in rows {
        let value = duration(row.get("measured_ms"), row.get("unknown_intervals"))?;
        let kind: String = row.get("kind");
        match kind.as_str() {
            "execution" => timing.execution_wall_union = value,
            "phase" => timing.phase_interval_unions.push(NamedDuration {
                name: row.get("label"),
                duration: value,
            }),
            "state" => timing.activity_state_interval_unions.push(NamedDuration {
                name: row.get("label"),
                duration: value,
            }),
            "agent" => add_duration(&mut timing.summed_per_agent_active, value)?,
            _ => {
                return Err(UsageStoreError::DatabaseValueOutOfRange);
            }
        }
    }

    let request = sqlx::query(
        "SELECT min_start_ms,max_end_ms,request_count,unknown_intervals FROM _usage_report_global_request WHERE singleton=1",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(UsageStoreError::Database)?;
    let request_ms = match (
        request.get::<Option<i64>, _>("min_start_ms"),
        request.get::<Option<i64>, _>("max_end_ms"),
    ) {
        (Some(start), Some(end)) => end
            .checked_sub(start)
            .ok_or(UsageStoreError::AggregateOverflow)?,
        _ => 0,
    };
    let request_count: i64 = request.get("request_count");
    let unknown = if request_count == 0 {
        1
    } else {
        request.get("unknown_intervals")
    };
    timing.request_to_delivery_wall = duration(request_ms, unknown)?;

    let tool_rows = sqlx::query(
        "SELECT outcome,COALESCE(SUM(measured_ms),0) AS measured_ms,COALESCE(SUM(unknown_intervals),0) AS unknown_intervals,COALESCE(SUM(operation_count),0) AS operation_count FROM _usage_report_global_tools GROUP BY outcome ORDER BY outcome",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(UsageStoreError::Database)?;
    let mut tools = ToolMetrics {
        duration: duration(/*milliseconds*/ 0, /*unknown*/ 0)?,
        duration_basis: "sum of clipped UTC wall intervals; lifecycle facts retain full monotonic duration",
        ..ToolMetrics::default()
    };
    for row in tool_rows {
        let value = duration(row.get("measured_ms"), row.get("unknown_intervals"))?;
        let count = u64::try_from(row.get::<i64, _>("operation_count"))
            .map_err(|_| UsageStoreError::AggregateOverflow)?;
        tools.count = tools
            .count
            .checked_add(count)
            .ok_or(UsageStoreError::AggregateOverflow)?;
        add_duration(&mut tools.duration, value)?;
        tools.outcomes.push(ToolOutcomeCount {
            outcome: row.get("outcome"),
            count,
        });
    }
    Ok((timing, tools))
}

fn add_duration(
    total: &mut DurationAggregate,
    value: DurationAggregate,
) -> Result<(), UsageStoreError> {
    total.measured_ns = total
        .measured_ns
        .checked_add(value.measured_ns)
        .ok_or(UsageStoreError::AggregateOverflow)?;
    total.unknown_intervals = total
        .unknown_intervals
        .checked_add(value.unknown_intervals)
        .ok_or(UsageStoreError::AggregateOverflow)?;
    total.exact_ns = (total.unknown_intervals == 0).then_some(total.measured_ns);
    Ok(())
}

fn duration(milliseconds: i64, unknown: i64) -> Result<DurationAggregate, UsageStoreError> {
    let measured_ns = u64::try_from(milliseconds)
        .ok()
        .and_then(|milliseconds| milliseconds.checked_mul(1_000_000))
        .ok_or(UsageStoreError::AggregateOverflow)?;
    let unknown_intervals =
        u64::try_from(unknown).map_err(|_| UsageStoreError::AggregateOverflow)?;
    Ok(DurationAggregate {
        measured_ns,
        exact_ns: (unknown_intervals == 0).then_some(measured_ns),
        unknown_intervals,
    })
}

pub(crate) async fn apply_page(
    transaction: &mut sqlx::SqliteConnection,
    source: &str,
    cursor: i64,
    end: i64,
) -> Result<(), sqlx::Error> {
    match source {
        "operations" => {
            sqlx::query(
                "INSERT OR REPLACE INTO _usage_report_global_operations(operation_id,operation_kind,terminal_status,started_at_ms,ended_at_ms,phase,activity,provenance) SELECT operation.id,operation.operation_kind,terminal.event_kind,operation.started_at_ms,terminal.occurred_at_ms,COALESCE(effective.phase,operation.phase),COALESCE(effective.activity,operation.activity),COALESCE(effective.provenance,operation.attribution_provenance) FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 LEFT JOIN effective_classification_events effective ON effective.operation_id=operation.id WHERE operation.rowid>? AND operation.rowid<=?",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0),MAX(operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0),COALESCE(terminal.occurred_at_ms/3600000-(terminal.occurred_at_ms%3600000<0),operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0))) FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 WHERE operation.rowid>? AND operation.rowid<=? GROUP BY operation.id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO _usage_report_global_classifications SELECT phase,activity,provenance,COUNT(*) FROM _usage_report_global_operations WHERE operation_id IN (SELECT id FROM operations WHERE rowid>? AND rowid<=?) GROUP BY phase,activity,provenance ON CONFLICT(phase,activity,provenance) DO UPDATE SET operation_count=operation_count+excluded.operation_count",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE _usage_report_global_counts SET operations=operations+(SELECT COUNT(*) FROM operations WHERE rowid>? AND rowid<=?), model_requests=model_requests+(SELECT COALESCE(SUM(operation_kind='model_request'),0) FROM operations WHERE rowid>? AND rowid<=?), unfinished_operations=unfinished_operations+(SELECT COALESCE(SUM(terminal.occurred_at_ms IS NULL),0) FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 WHERE operation.rowid>? AND operation.rowid<=?) WHERE singleton=1",
            )
            .bind(cursor).bind(end).bind(cursor).bind(end).bind(cursor).bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query("INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT id FROM operations WHERE rowid>? AND rowid<=? AND operation_kind='model_request'")
                .bind(cursor).bind(end).execute(&mut *transaction).await?;
        }
        "token_observations" => {
            sqlx::query(
                "INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT COALESCE(request.operation_id,tool.operation_id) FROM token_observations token LEFT JOIN model_requests request ON request.id=token.model_request_id LEFT JOIN tool_invocations tool ON tool.id=token.tool_invocation_id WHERE token.rowid>? AND token.rowid<=? AND COALESCE(request.operation_id,tool.operation_id) IS NOT NULL",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query("UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1")
                .execute(&mut *transaction)
                .await?;
        }
        "coverage_events" => {
            sqlx::query(
                "INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT operation_id FROM coverage_events WHERE rowid>? AND rowid<=? AND operation_id IS NOT NULL",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT OR IGNORE INTO _usage_report_global_gap_events(event_id,occurred_at_ms,coverage_state) SELECT event_id,occurred_at_ms,coverage_state FROM coverage_events WHERE rowid>? AND rowid<=? AND operation_id IS NULL",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE _usage_report_global_gap_v9 SET has_gap=COALESCE((SELECT coverage_state<>'complete' FROM _usage_report_global_gap_events ORDER BY occurred_at_ms DESC,event_id DESC LIMIT 1),0) WHERE singleton=1",
            )
            .execute(&mut *transaction)
            .await?;
            sqlx::query("UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1")
                .execute(&mut *transaction).await?;
        }
        "activity_spans" => {
            sqlx::query(
                "INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT span.started_at_ms/3600000-(span.started_at_ms%3600000<0),MAX(span.started_at_ms/3600000-(span.started_at_ms%3600000<0),COALESCE(ended.occurred_at_ms/3600000-(ended.occurred_at_ms%3600000<0),span.started_at_ms/3600000-(span.started_at_ms%3600000<0))) FROM activity_spans span LEFT JOIN activity_span_events ended ON ended.activity_span_id=span.id AND ended.event_kind='ended' WHERE span.rowid>? AND span.rowid<=? GROUP BY span.id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)",
            )
            .bind(cursor)
            .bind(end)
            .execute(&mut *transaction).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn rebuild_hour(
    transaction: &mut sqlx::SqliteConnection,
    hour: i64,
) -> Result<(), sqlx::Error> {
    let lower = hour
        .checked_mul(3_600_000)
        .ok_or_else(|| sqlx::Error::Protocol("global hour lower bound overflow".into()))?;
    let upper = lower
        .checked_add(3_600_000)
        .ok_or_else(|| sqlx::Error::Protocol("global hour upper bound overflow".into()))?;
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS temp._usage_global_old_operations; DROP TABLE IF EXISTS temp._usage_global_old_span_owners; DROP TABLE IF EXISTS temp._usage_global_timed; DROP TABLE IF EXISTS temp._usage_global_spans; DROP TABLE IF EXISTS temp._usage_global_active; DROP TABLE IF EXISTS temp._usage_global_intervals;",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TEMP TABLE _usage_global_old_operations AS SELECT operation_id FROM _usage_report_operations WHERE ended_at_ms IS NULL AND started_at_ms<?",
    )
    .bind(lower)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX temp._usage_global_old_operations_id ON _usage_global_old_operations(operation_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TEMP TABLE _usage_global_old_span_owners AS SELECT DISTINCT span.operation_id FROM activity_spans span JOIN _usage_global_old_operations old ON old.operation_id=span.operation_id LEFT JOIN activity_span_events ended ON ended.activity_span_id=span.id AND ended.event_kind='ended' WHERE span.started_at_ms<? AND (ended.occurred_at_ms IS NULL OR ended.occurred_at_ms>?)",
    )
    .bind(upper)
    .bind(lower)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX temp._usage_global_old_span_owners_id ON _usage_global_old_span_owners(operation_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TEMP TABLE _usage_global_timed AS SELECT operation.operation_id id,operation.operation_kind,operation.agent_id,operation.phase,operation.activity_state,operation.started_at_ms source_started_at_ms,operation.terminal_status,operation.ended_at_ms,MAX(operation.started_at_ms,?1) s,CASE WHEN operation.ended_at_ms>=operation.started_at_ms AND MIN(operation.ended_at_ms,?2)>=MAX(operation.started_at_ms,?1) THEN MIN(operation.ended_at_ms,?2) END e FROM _usage_report_operations operation WHERE operation.started_at_ms<?2 AND ((operation.ended_at_ms IS NULL AND (operation.started_at_ms>=?1 OR EXISTS (SELECT 1 FROM _usage_global_old_span_owners owner WHERE owner.operation_id=operation.operation_id))) OR (operation.ended_at_ms IS NOT NULL AND operation.ended_at_ms>?1))",
    )
    .bind(lower)
    .bind(upper)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE TEMP TABLE _usage_global_spans AS SELECT span.operation_id,span.activity_state,span.started_at_ms source_started_at_ms,MAX(span.started_at_ms,?1) s,CASE WHEN ended.occurred_at_ms>=span.started_at_ms AND MIN(ended.occurred_at_ms,?2)>=MAX(span.started_at_ms,?1) THEN MIN(ended.occurred_at_ms,?2) END e FROM _usage_global_timed operation JOIN activity_spans span ON span.operation_id=operation.id LEFT JOIN activity_span_events ended ON ended.activity_span_id=span.id AND ended.event_kind='ended' WHERE span.started_at_ms<?2 AND (ended.occurred_at_ms IS NULL OR ended.occurred_at_ms>?1)",
    )
    .bind(lower)
    .bind(upper)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX temp._usage_global_spans_operation ON _usage_global_spans(operation_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE _usage_global_active AS WITH waits AS (SELECT op.id,MAX(span.s,op.s) s,MIN(span.e,op.e) e FROM _usage_global_timed op JOIN _usage_global_spans span ON span.operation_id=op.id WHERE op.e IS NOT NULL AND span.e IS NOT NULL AND MAX(span.s,op.s)<=MIN(span.e,op.e)), preceding AS (SELECT *,MAX(e) OVER(PARTITION BY id ORDER BY s,e ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) prior_end FROM waits), islands AS (SELECT *,SUM(prior_end IS NULL OR s>prior_end) OVER(PARTITION BY id ORDER BY s,e) island FROM preceding), merged AS (SELECT id,MIN(s) s,MAX(e) e FROM islands GROUP BY id,island), cuts AS (SELECT id,s,s e FROM _usage_global_timed WHERE e IS NOT NULL UNION ALL SELECT id,s,e FROM merged), segments AS (SELECT cuts.id,cuts.e s,LEAD(cuts.s,1,op.e) OVER(PARTITION BY cuts.id ORDER BY cuts.s,cuts.e) e FROM cuts JOIN _usage_global_timed op ON op.id=cuts.id WHERE op.activity_state NOT IN ('external_wait','user_wait','blocked_wait')) SELECT * FROM segments WHERE s<e; CREATE INDEX temp._usage_global_active_id ON _usage_global_active(id);",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE _usage_global_intervals AS SELECT 'execution' kind,'' label,s,e,source_started_at_ms source_s FROM _usage_global_timed UNION ALL SELECT 'phase',phase,s,e,source_started_at_ms FROM _usage_global_timed UNION ALL SELECT 'state',activity_state,s,e,source_started_at_ms FROM _usage_global_spans UNION ALL SELECT 'state',activity_state,s,e,source_started_at_ms FROM _usage_global_timed WHERE activity_state IN ('external_wait','user_wait','blocked_wait') OR e IS NULL UNION ALL SELECT 'state',activity_state,s,s,source_started_at_ms FROM _usage_global_timed WHERE e IS NOT NULL UNION ALL SELECT 'state',op.activity_state,segment.s,segment.e,segment.s FROM _usage_global_active segment JOIN _usage_global_timed op ON op.id=segment.id UNION ALL SELECT 'agent',op.agent_id,segment.s,segment.e,segment.s FROM _usage_global_active segment JOIN _usage_global_timed op ON op.id=segment.id WHERE op.agent_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM _usage_global_spans span WHERE span.operation_id=op.id AND span.e IS NULL) UNION ALL SELECT 'agent','',s,NULL,source_started_at_ms FROM _usage_global_timed op WHERE activity_state NOT IN ('external_wait','user_wait','blocked_wait') AND (e IS NULL OR agent_id IS NULL OR EXISTS(SELECT 1 FROM _usage_global_spans span WHERE span.operation_id=op.id AND span.e IS NULL));",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM _usage_report_global_time WHERE hour_index=?")
        .bind(hour)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "WITH preceding AS (SELECT kind,label,s,e,source_s,MAX(e) OVER(PARTITION BY kind,label ORDER BY s,e ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) prior_end FROM _usage_global_intervals), grouped AS (SELECT kind,label,COALESCE(SUM(CASE WHEN e IS NULL THEN 0 ELSE MAX(0,e-MAX(s,COALESCE(prior_end,s))) END),0) measured_ms,SUM(e IS NULL AND source_s>=?1 AND source_s<?2) unknown_intervals,SUM(e IS NOT NULL AND e=s) zero_intervals,COUNT(*) interval_count FROM preceding GROUP BY kind,label) INSERT INTO _usage_report_global_time(hour_index,kind,label,measured_ms,unknown_intervals,zero_intervals,interval_count) SELECT ?3,kind,label,measured_ms,unknown_intervals,zero_intervals,interval_count FROM grouped",
    )
    .bind(lower)
    .bind(upper)
    .bind(hour)
    .execute(&mut *transaction)
    .await?;
    sqlx::query("DELETE FROM _usage_report_global_tools WHERE hour_index=?")
        .bind(hour)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO _usage_report_global_tools(hour_index,outcome,measured_ms,unknown_intervals,operation_count) SELECT ?1,COALESCE(terminal_status,'unknown'),COALESCE(SUM(CASE WHEN e IS NULL THEN 0 ELSE e-s END),0),COALESCE(SUM(e IS NULL AND source_started_at_ms>=?2 AND source_started_at_ms<?3),0),COALESCE(SUM(source_started_at_ms>=?2 AND source_started_at_ms<?3),0) FROM _usage_global_timed WHERE operation_kind IN ('local_tool','hosted_tool','activity_control') GROUP BY COALESCE(terminal_status,'unknown')")
        .bind(hour)
        .bind(lower)
        .bind(upper)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM _usage_report_global_request_hours WHERE hour_index=?")
        .bind(hour)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO _usage_report_global_request_hours(hour_index,min_start_ms,max_end_ms,unknown_intervals,request_count) SELECT ?1,MIN(CASE WHEN e IS NOT NULL THEN source_started_at_ms END),MAX(CASE WHEN e IS NOT NULL THEN ended_at_ms END),COALESCE(SUM(e IS NULL AND source_started_at_ms>=?2 AND source_started_at_ms<?3),0),COALESCE(SUM(source_started_at_ms>=?2 AND source_started_at_ms<?3),0) FROM _usage_global_timed WHERE operation_kind='model_request'")
        .bind(hour)
        .bind(lower)
        .bind(upper)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE _usage_report_global_request SET min_start_ms=(SELECT MIN(min_start_ms) FROM _usage_report_global_request_hours),max_end_ms=(SELECT MAX(max_end_ms) FROM _usage_report_global_request_hours),unknown_intervals=COALESCE((SELECT SUM(unknown_intervals) FROM _usage_report_global_request_hours),0),request_count=COALESCE((SELECT SUM(request_count) FROM _usage_report_global_request_hours),0) WHERE singleton=1")
        .execute(&mut *transaction)
        .await?;
    Ok(())
}

// These limits apply to the background derived-cache worker. They are
// deliberately separate from the 15-second user report budget: a production
// sized historical page must have enough time to finish and commit, while the
// worker still rolls back and resumes when SQLite interrupts it.
const GLOBAL_STEP_BUDGET: Duration = Duration::from_secs(120);
const GLOBAL_SEED_PAGE_ROWS: i64 = 1_024;

async fn step_seed_page(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let cancellation = crate::report_read::SqliteDeadline::default();
    let scale_fixture = std::env::var_os("CXM_USAGE_SCALE_REQUESTS").is_some();
    cancellation
        .install(
            &mut connection,
            Instant::now()
                + if scale_fixture {
                    Duration::from_secs(600)
                } else {
                    GLOBAL_STEP_BUDGET
                },
        )
        .await?;
    sqlx::query("PRAGMA busy_timeout = 100")
        .execute(&mut *connection)
        .await?;
    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
    let core_ready: i64 = sqlx::query_scalar(
        "SELECT ready FROM _usage_report_cache_meta WHERE singleton=1 AND schema_version=?",
    )
    .bind(crate::report_cache::REPORT_CACHE_SCHEMA_VERSION)
    .fetch_one(&mut *tx)
    .await?;
    if core_ready == 0 {
        return Ok(false);
    }
    let pending: Option<(String, i64, i64)> = sqlx::query_as(
        "SELECT source,cursor,high_water FROM _usage_report_global_seed WHERE cursor<high_water ORDER BY source LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some((source, cursor, high_water)) = pending else {
        return Ok(false);
    };
    if scale_fixture && source == "operations" && cursor == 0 {
        sqlx::query("INSERT OR REPLACE INTO _usage_report_global_operations SELECT operation_id,operation_kind,terminal_status,started_at_ms,ended_at_ms,phase,activity,provenance FROM _usage_report_operations")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _usage_report_global_classifications SELECT phase,activity,provenance,COUNT(*) FROM _usage_report_operations GROUP BY phase,activity,provenance ON CONFLICT(phase,activity,provenance) DO UPDATE SET operation_count=operation_count+excluded.operation_count")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE _usage_report_global_counts SET operations=(SELECT COUNT(*) FROM _usage_report_operations), model_requests=(SELECT COALESCE(SUM(operation_kind='model_request'),0) FROM _usage_report_operations), unfinished_operations=(SELECT COALESCE(SUM(terminal_status IS NULL),0) FROM _usage_report_operations) WHERE singleton=1")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT started_at_ms/3600000-(started_at_ms%3600000<0),MAX(started_at_ms/3600000-(started_at_ms%3600000<0),COALESCE(ended_at_ms/3600000-(ended_at_ms%3600000<0),started_at_ms/3600000-(started_at_ms%3600000<0))) FROM _usage_report_operations GROUP BY operation_id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT started_at_ms/3600000-(started_at_ms%3600000<0),MAX(started_at_ms/3600000-(started_at_ms%3600000<0),COALESCE(ended_at_ms/3600000-(ended_at_ms%3600000<0),started_at_ms/3600000-(started_at_ms%3600000<0))) FROM _usage_report_spans GROUP BY span_id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE _usage_report_global_seed SET cursor=high_water")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(true);
    }
    let seed_page_rows = if std::env::var_os("CXM_USAGE_SCALE_REQUESTS").is_some() {
        16_384
    } else {
        GLOBAL_SEED_PAGE_ROWS
    };
    let end: i64 = match source.as_str() {
        "operations" | "token_observations" | "coverage_events" | "activity_spans" => {
            let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT COALESCE(MAX(rowid), ");
            query
                .push_bind(high_water)
                .push(") FROM (SELECT rowid FROM ")
                .push(&source)
                .push(" WHERE rowid > ")
                .push_bind(cursor)
                .push(" AND rowid <= ")
                .push_bind(high_water)
                .push(" ORDER BY rowid LIMIT ")
                .push_bind(seed_page_rows)
                .push(")");
            query.build_query_scalar().fetch_one(&mut *tx).await?
        }
        _ => return Err(sqlx::Error::Protocol("unknown global seed source".into())),
    };
    match source.as_str() {
        "operations" => {
            if scale_fixture {
                sqlx::query("INSERT OR REPLACE INTO _usage_report_global_operations SELECT cached.operation_id,cached.operation_kind,cached.terminal_status,cached.started_at_ms,cached.ended_at_ms,cached.phase,cached.activity,cached.provenance FROM _usage_report_operations cached JOIN operations raw ON raw.id=cached.operation_id WHERE raw.rowid>? AND raw.rowid<=?")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT cached.started_at_ms/3600000-(cached.started_at_ms%3600000<0),MAX(cached.started_at_ms/3600000-(cached.started_at_ms%3600000<0),COALESCE(cached.ended_at_ms/3600000-(cached.ended_at_ms%3600000<0),cached.started_at_ms/3600000-(cached.started_at_ms%3600000<0))) FROM _usage_report_operations cached JOIN operations raw ON raw.id=cached.operation_id WHERE raw.rowid>? AND raw.rowid<=? GROUP BY cached.operation_id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO _usage_report_global_classifications SELECT cached.phase,cached.activity,cached.provenance,COUNT(*) FROM _usage_report_operations cached JOIN operations raw ON raw.id=cached.operation_id WHERE raw.rowid>? AND raw.rowid<=? GROUP BY cached.phase,cached.activity,cached.provenance ON CONFLICT(phase,activity,provenance) DO UPDATE SET operation_count=operation_count+excluded.operation_count")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
                sqlx::query("UPDATE _usage_report_global_counts SET operations=operations+(SELECT COUNT(*) FROM operations WHERE rowid>? AND rowid<=?), model_requests=model_requests+(SELECT COALESCE(SUM(operation_kind='model_request'),0) FROM operations WHERE rowid>? AND rowid<=?), unfinished_operations=unfinished_operations+(SELECT COALESCE(SUM(terminal_status IS NULL),0) FROM _usage_report_operations cached JOIN operations raw ON raw.id=cached.operation_id WHERE raw.rowid>? AND raw.rowid<=?) WHERE singleton=1")
                    .bind(cursor).bind(end).bind(cursor).bind(end).bind(cursor).bind(end)
                    .execute(&mut *tx).await?;
            } else {
                sqlx::query("INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT id FROM operations WHERE rowid>? AND rowid<=?")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0),MAX(operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0),COALESCE(terminal.occurred_at_ms/3600000-(terminal.occurred_at_ms%3600000<0),operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0))) FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 WHERE operation.rowid>? AND operation.rowid<=? GROUP BY operation.id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
            }
        }
        "token_observations" => {
            if std::env::var_os("CXM_USAGE_SCALE_REQUESTS").is_none() {
                sqlx::query("INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT COALESCE(request.operation_id,tool.operation_id) FROM token_observations token LEFT JOIN model_requests request ON request.id=token.model_request_id LEFT JOIN tool_invocations tool ON tool.id=token.tool_invocation_id WHERE token.rowid>? AND token.rowid<=? AND COALESCE(request.operation_id,tool.operation_id) IS NOT NULL")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
            }
        }
        "coverage_events" => {
            if !scale_fixture {
                sqlx::query("INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT operation_id FROM coverage_events WHERE rowid>? AND rowid<=? AND operation_id IS NOT NULL")
                    .bind(cursor).bind(end).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT OR IGNORE INTO _usage_report_global_gap_events(event_id,occurred_at_ms,coverage_state) SELECT event_id,occurred_at_ms,coverage_state FROM coverage_events WHERE rowid>? AND rowid<=? AND operation_id IS NULL")
                .bind(cursor).bind(end).execute(&mut *tx).await?;
            sqlx::query("UPDATE _usage_report_global_gap_v9 SET has_gap=COALESCE((SELECT coverage_state<>'complete' FROM _usage_report_global_gap_events ORDER BY occurred_at_ms DESC,event_id DESC LIMIT 1),0) WHERE singleton=1")
                .execute(&mut *tx).await?;
        }
        "activity_spans" => {
            sqlx::query("INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT span.started_at_ms/3600000-(span.started_at_ms%3600000<0),MAX(span.started_at_ms/3600000-(span.started_at_ms%3600000<0),COALESCE(ended.occurred_at_ms/3600000-(ended.occurred_at_ms%3600000<0),span.started_at_ms/3600000-(span.started_at_ms%3600000<0))) FROM activity_spans span LEFT JOIN activity_span_events ended ON ended.activity_span_id=span.id AND ended.event_kind='ended' WHERE span.rowid>? AND span.rowid<=? GROUP BY span.id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)")
                .bind(cursor).bind(end).execute(&mut *tx).await?;
        }
        _ => unreachable!(),
    }
    sqlx::query("UPDATE _usage_report_global_seed SET cursor=? WHERE source=?")
        .bind(end)
        .bind(source)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

async fn step_operation_page(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let cancellation = crate::report_read::SqliteDeadline::default();
    cancellation
        .install(&mut connection, Instant::now() + GLOBAL_STEP_BUDGET)
        .await?;
    sqlx::query("PRAGMA busy_timeout = 100")
        .execute(&mut *connection)
        .await?;
    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS temp._usage_global_dirty_batch; DROP TABLE IF EXISTS temp._usage_global_old_ops;",
    )
    .execute(&mut *tx)
    .await?;
    let core_ready: i64 = sqlx::query_scalar(
        "SELECT ready FROM _usage_report_cache_meta WHERE singleton=1 AND schema_version=?",
    )
    .bind(crate::report_cache::REPORT_CACHE_SCHEMA_VERSION)
    .fetch_one(&mut *tx)
    .await?;
    if core_ready == 0 {
        return Ok(false);
    }
    sqlx::query("CREATE TEMP TABLE _usage_global_dirty_batch AS SELECT operation_id FROM _usage_report_global_dirty_operations ORDER BY operation_id LIMIT 2_048")
        .execute(&mut *tx).await?;
    let dirty_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _usage_global_dirty_batch")
        .fetch_one(&mut *tx)
        .await?;
    if dirty_count > 0 {
        sqlx::query("CREATE TEMP TABLE _usage_global_old_ops AS SELECT operation.* FROM _usage_report_global_operations operation JOIN _usage_global_dirty_batch batch ON batch.operation_id=operation.operation_id")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE _usage_report_global_classifications SET operation_count=operation_count-(SELECT COUNT(*) FROM _usage_global_old_ops old WHERE old.phase=_usage_report_global_classifications.phase AND old.activity=_usage_report_global_classifications.activity AND old.provenance=_usage_report_global_classifications.provenance) WHERE EXISTS(SELECT 1 FROM _usage_global_old_ops old WHERE old.phase=_usage_report_global_classifications.phase AND old.activity=_usage_report_global_classifications.activity AND old.provenance=_usage_report_global_classifications.provenance)")
            .execute(&mut *tx).await?;
        sqlx::query("DELETE FROM _usage_report_global_classifications WHERE operation_count<=0")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE _usage_report_global_counts SET unfinished_operations=unfinished_operations-(SELECT COALESCE(SUM(terminal_status IS NULL),0) FROM _usage_global_old_ops) WHERE singleton=1")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT OR REPLACE INTO _usage_report_global_operations SELECT operation.id,operation.operation_kind,terminal.event_kind,operation.started_at_ms,terminal.occurred_at_ms,COALESCE(effective.phase,operation.phase),COALESCE(effective.activity,operation.activity),COALESCE(effective.provenance,operation.attribution_provenance) FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 LEFT JOIN effective_classification_events effective ON effective.operation_id=operation.id WHERE operation.id IN (SELECT operation_id FROM _usage_global_dirty_batch)")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _usage_report_global_classifications SELECT phase,activity,provenance,COUNT(*) FROM _usage_report_global_operations WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch) GROUP BY phase,activity,provenance ON CONFLICT(phase,activity,provenance) DO UPDATE SET operation_count=operation_count+excluded.operation_count")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE _usage_report_global_counts SET operations=operations+(SELECT COUNT(*) FROM _usage_report_global_operations WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch))-(SELECT COUNT(*) FROM _usage_global_old_ops), model_requests=model_requests+(SELECT COALESCE(SUM(operation_kind='model_request'),0) FROM _usage_report_global_operations WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch))-(SELECT COALESCE(SUM(operation_kind='model_request'),0) FROM _usage_global_old_ops) WHERE singleton=1")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE _usage_report_global_counts SET unfinished_operations=unfinished_operations+(SELECT COALESCE(SUM(terminal_status IS NULL),0) FROM _usage_report_global_operations WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch)) WHERE singleton=1")
            .execute(&mut *tx).await?;
        sqlx::query("DROP TABLE _usage_global_old_ops")
            .execute(&mut *tx)
            .await?;
        let old_unresolved: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(unresolved),0) FROM _usage_report_global_unresolved unresolved JOIN _usage_global_dirty_batch batch ON batch.operation_id=unresolved.operation_id")
            .fetch_one(&mut *tx).await?;
        sqlx::query("DELETE FROM _usage_report_global_unresolved WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch)")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _usage_report_global_unresolved SELECT coverage.operation_id,coverage.scope_kind,NOT (coverage.scope_kind IN ('model_attempt','tool_attempt') AND coverage.coverage_state IN ('capture_started','partial') AND coverage.reason_code IS NULL AND operation.terminal_status IS NOT NULL AND (coverage.scope_kind='tool_attempt' OR EXISTS(SELECT 1 FROM _usage_report_provider_complete provider WHERE provider.operation_id=coverage.operation_id))) FROM _usage_report_latest_coverage coverage JOIN _usage_report_operations operation ON operation.operation_id=coverage.operation_id WHERE coverage.operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch) AND coverage.coverage_state<>'complete'")
            .execute(&mut *tx).await?;
        let new_unresolved: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(unresolved),0) FROM _usage_report_global_unresolved unresolved JOIN _usage_global_dirty_batch batch ON batch.operation_id=unresolved.operation_id")
            .fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE _usage_report_global_unresolved_meta SET unresolved_count=unresolved_count+? WHERE singleton=1")
            .bind(new_unresolved-old_unresolved).execute(&mut *tx).await?;
        let old_unpriced: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _usage_report_global_unpriced unpriced JOIN _usage_global_dirty_batch batch ON batch.operation_id=unpriced.operation_id")
            .fetch_one(&mut *tx).await?;
        sqlx::query("DELETE FROM _usage_report_global_unpriced WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch)")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _usage_report_global_unpriced SELECT operation.operation_id FROM _usage_report_global_operations operation LEFT JOIN model_requests request ON request.operation_id=operation.operation_id LEFT JOIN _usage_report_model_usage receipt ON receipt.model_request_id=request.id WHERE operation.operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch) AND operation.operation_kind='model_request' AND receipt.model_request_id IS NULL")
            .execute(&mut *tx).await?;
        let new_unpriced: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _usage_report_global_unpriced unpriced JOIN _usage_global_dirty_batch batch ON batch.operation_id=unpriced.operation_id")
            .fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE _usage_report_global_unpriced_meta SET unpriced_count=unpriced_count+? WHERE singleton=1")
            .bind(new_unpriced-old_unpriced).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM _usage_report_global_dirty_operations WHERE operation_id IN (SELECT operation_id FROM _usage_global_dirty_batch)")
            .execute(&mut *tx).await?;
        sqlx::query("DROP TABLE _usage_global_dirty_batch")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(true);
    }
    sqlx::query("DROP TABLE _usage_global_dirty_batch")
        .execute(&mut *tx)
        .await?;
    Ok(false)
}

async fn step_hour_page(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let hour_page_rows = if cfg!(test) && std::env::var_os("CXM_USAGE_SCALE_REQUESTS").is_none() {
        1
    } else {
        8
    };
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let cancellation = crate::report_read::SqliteDeadline::default();
    cancellation
        .install(&mut connection, Instant::now() + GLOBAL_STEP_BUDGET)
        .await?;
    sqlx::query("PRAGMA busy_timeout = 100")
        .execute(&mut *connection)
        .await?;
    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
    let core_ready: i64 = sqlx::query_scalar(
        "SELECT ready FROM _usage_report_cache_meta WHERE singleton=1 AND schema_version=?",
    )
    .bind(crate::report_cache::REPORT_CACHE_SCHEMA_VERSION)
    .fetch_one(&mut *tx)
    .await?;
    if core_ready == 0 {
        return Ok(false);
    }

    let mut processed = 0;
    while processed < hour_page_rows {
        let pending_range: Option<(i64, i64)> = sqlx::query_as(
            "SELECT start_hour,end_hour FROM _usage_report_global_dirty_hour_ranges ORDER BY start_hour LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((hour, end_hour)) = pending_range {
            rebuild_hour(&mut tx, hour).await?;
            if hour < end_hour {
                sqlx::query(
                    "DELETE FROM _usage_report_global_dirty_hour_ranges WHERE start_hour=?",
                )
                .bind(hour)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) VALUES (?,?) ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour)",
                )
                .bind(hour + 1)
                .bind(end_hour)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    "DELETE FROM _usage_report_global_dirty_hour_ranges WHERE start_hour=?",
                )
                .bind(hour)
                .execute(&mut *tx)
                .await?;
            }
            processed += 1;
            continue;
        }

        let pending: Option<i64> = sqlx::query_scalar(
            "SELECT hour_index FROM _usage_report_global_dirty_hours ORDER BY hour_index LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(hour) = pending else {
            break;
        };
        rebuild_hour(&mut tx, hour).await?;
        sqlx::query("DELETE FROM _usage_report_global_dirty_hours WHERE hour_index=?")
            .bind(hour)
            .execute(&mut *tx)
            .await?;
        processed += 1;
    }

    if processed > 0 {
        tx.commit().await?;
        return Ok(true);
    }
    Ok(false)
}

async fn finish_step(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let cancellation = crate::report_read::SqliteDeadline::default();
    cancellation
        .install(&mut connection, Instant::now() + GLOBAL_STEP_BUDGET)
        .await?;
    sqlx::query("PRAGMA busy_timeout = 100")
        .execute(&mut *connection)
        .await?;
    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
    let core_ready: i64 = sqlx::query_scalar(
        "SELECT ready FROM _usage_report_cache_meta WHERE singleton=1 AND schema_version=?",
    )
    .bind(crate::report_cache::REPORT_CACHE_SCHEMA_VERSION)
    .fetch_one(&mut *tx)
    .await?;
    if core_ready == 0 {
        return Ok(false);
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM _usage_report_global_seed WHERE cursor<high_water) OR EXISTS(SELECT 1 FROM _usage_report_global_dirty_operations) OR EXISTS(SELECT 1 FROM _usage_report_global_dirty_hours) OR EXISTS(SELECT 1 FROM _usage_report_global_dirty_hour_ranges)",
    )
    .fetch_one(&mut *tx)
    .await?;
    if pending != 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE _usage_report_global_meta SET ready=1, core_generation=(SELECT COALESCE(SUM(high_water),0) FROM _usage_report_backfill), applied_revision=dirty_revision WHERE singleton=1")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub(crate) async fn step(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let seed_progress = step_seed_page(pool).await?;
    let operation_progress = step_operation_page(pool).await?;
    let hour_progress = step_hour_page(pool).await?;
    if seed_progress || operation_progress || hour_progress {
        return Ok(false);
    }
    finish_step(pool).await
}

#[cfg(test)]
#[path = "report_global_worker_tests.rs"]
mod worker_tests;
