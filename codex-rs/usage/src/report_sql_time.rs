//! Exact interval unions and wait subtraction, sorted by SQLite rather than
//! retaining every operation and span in Rust collections.
use crate::{DurationAggregate, NamedDuration, ReportTimeMetrics, ToolMetrics, ToolOutcomeCount, UsageStoreError, UtcTimeRange};
use sqlx::{Row, SqliteConnection};

const INTERVALS: &str = r#"
CREATE TEMP TABLE _usage_active AS
WITH waits AS (
    SELECT op.id, MAX(span.s, op.s) AS s, MIN(span.e, op.e) AS e
    FROM _usage_selected AS op JOIN _usage_spans AS span ON span.operation_id = op.id
    WHERE op.e IS NOT NULL AND span.e IS NOT NULL AND MAX(span.s, op.s) <= MIN(span.e, op.e)
), preceding AS (
    SELECT *, MAX(e) OVER (PARTITION BY id ORDER BY s, e ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS prior_end FROM waits
), islands AS (
    SELECT *, SUM(prior_end IS NULL OR s > prior_end) OVER (PARTITION BY id ORDER BY s, e) AS island FROM preceding
), merged AS (
    SELECT id, MIN(s) AS s, MAX(e) AS e FROM islands GROUP BY id, island
), cuts AS (
    SELECT id, s, s AS e FROM _usage_selected WHERE e IS NOT NULL
    UNION ALL SELECT id, s, e FROM merged
), segments AS (
    SELECT cuts.id, cuts.e AS s,
           LEAD(cuts.s, 1, op.e) OVER (PARTITION BY cuts.id ORDER BY cuts.s, cuts.e) AS e
    FROM cuts JOIN _usage_selected AS op ON op.id = cuts.id
    WHERE op.activity_state NOT IN ('external_wait','user_wait','blocked_wait')
)
SELECT * FROM segments WHERE s < e;
CREATE INDEX temp._usage_active_id ON _usage_active(id);
CREATE TEMP TABLE _usage_intervals AS
SELECT 'execution' AS kind, '' AS label, s, e FROM _usage_selected
UNION ALL SELECT 'phase', phase, s, e FROM _usage_selected
UNION ALL SELECT 'state', activity_state, s, e FROM _usage_spans
UNION ALL SELECT 'state', activity_state, s, e FROM _usage_selected
    WHERE activity_state IN ('external_wait','user_wait','blocked_wait') OR e IS NULL
UNION ALL SELECT 'state', activity_state, s, s FROM _usage_selected WHERE e IS NOT NULL
UNION ALL SELECT 'state', op.activity_state, segment.s, segment.e
    FROM _usage_active AS segment JOIN _usage_selected AS op ON op.id = segment.id
UNION ALL SELECT 'agent', op.agent_id, segment.s, segment.e
    FROM _usage_active AS segment JOIN _usage_selected AS op ON op.id = segment.id
    WHERE op.agent_id IS NOT NULL AND NOT EXISTS
        (SELECT 1 FROM _usage_spans AS span WHERE span.operation_id = op.id AND span.e IS NULL)
UNION ALL SELECT 'agent', '', s, NULL FROM _usage_selected AS op
    WHERE activity_state NOT IN ('external_wait','user_wait','blocked_wait')
      AND (e IS NULL OR agent_id IS NULL OR EXISTS
        (SELECT 1 FROM _usage_spans AS span WHERE span.operation_id = op.id AND span.e IS NULL));
"#;

pub(super) async fn metrics(connection: &mut SqliteConnection, range: Option<UtcTimeRange>) -> Result<(ReportTimeMetrics, ToolMetrics), UsageStoreError> {
    let start = range.map_or(i64::MIN, UtcTimeRange::start_ms);
    let end = range.map_or(i64::MAX, UtcTimeRange::end_ms);
    sqlx::raw_sql("ALTER TABLE _usage_selected ADD COLUMN s INTEGER; ALTER TABLE _usage_selected ADD COLUMN e INTEGER;")
        .execute(&mut *connection).await.map_err(super::database_error)?;
    sqlx::query("UPDATE _usage_selected SET s = MAX(started_at_ms, ?1), e = CASE WHEN ended_at_ms >= started_at_ms AND MIN(ended_at_ms, ?2) >= MAX(started_at_ms, ?1) THEN MIN(ended_at_ms, ?2) END")
        .bind(start).bind(end).execute(&mut *connection).await.map_err(super::database_error)?;
    sqlx::query(r#"
        CREATE TEMP TABLE _usage_spans AS
        SELECT span.operation_id, span.activity_state, MAX(span.started_at_ms, ?1) AS s,
               CASE WHEN ended.occurred_at_ms >= span.started_at_ms AND MIN(ended.occurred_at_ms, ?2) >= MAX(span.started_at_ms, ?1)
                    THEN MIN(ended.occurred_at_ms, ?2) END AS e
        FROM _usage_selected AS op JOIN activity_spans AS span ON span.operation_id = op.id
        LEFT JOIN activity_span_events AS ended ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
    "#).bind(start).bind(end).execute(&mut *connection).await.map_err(super::database_error)?;
    sqlx::query("CREATE INDEX temp._usage_spans_operation ON _usage_spans(operation_id)").execute(&mut *connection).await.map_err(super::database_error)?;
    sqlx::raw_sql(INTERVALS).execute(&mut *connection).await.map_err(super::database_error)?;
    let rows = sqlx::query(r#"
        WITH preceding AS (
            SELECT kind, label, s, e,
                   MAX(e) OVER (PARTITION BY kind, label ORDER BY s, e ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS prior_end
            FROM _usage_intervals
        ), grouped AS (
        SELECT kind, label,
               COALESCE(SUM(CASE WHEN e IS NULL THEN 0 ELSE MAX(0, e - MAX(s, COALESCE(prior_end, s))) END), 0) AS measured_ms,
               SUM(e IS NULL) AS unknown_intervals
        FROM preceding GROUP BY kind, label
        ) SELECT kind, CASE WHEN kind = 'agent' THEN '' ELSE label END AS label,
                 SUM(measured_ms) AS measured_ms, SUM(unknown_intervals) AS unknown_intervals
          FROM grouped GROUP BY kind, CASE WHEN kind = 'agent' THEN '' ELSE label END
          ORDER BY kind, label
    "#).fetch_all(&mut *connection).await.map_err(super::database_error)?;
    let mut timing = ReportTimeMetrics::default();
    timing.execution_wall_union = duration(/*milliseconds*/ 0, /*unknown*/ 0)?;
    timing.summed_per_agent_active = duration(/*milliseconds*/ 0, /*unknown*/ 0)?;
    for row in rows {
        let value = duration(row.try_get("measured_ms").map_err(|_| UsageStoreError::AggregateOverflow)?, row.get("unknown_intervals"))?;
        let kind: String = row.get("kind");
        match kind.as_str() {
            "execution" => timing.execution_wall_union = value,
            "phase" => timing.phase_interval_unions.push(NamedDuration {name:row.get("label"), duration:value}),
            "state" => timing.activity_state_interval_unions.push(NamedDuration {name:row.get("label"), duration:value}),
            "agent" => {
                let total = &mut timing.summed_per_agent_active;
                total.measured_ns = total.measured_ns.checked_add(value.measured_ns).ok_or(UsageStoreError::AggregateOverflow)?;
                total.unknown_intervals = total.unknown_intervals.checked_add(value.unknown_intervals).ok_or(UsageStoreError::AggregateOverflow)?;
                total.exact_ns = (total.unknown_intervals == 0).then_some(total.measured_ns);
            }
            _ => return Err(UsageStoreError::DatabaseValueOutOfRange),
        }
    }
    let request = sqlx::query("SELECT MIN(CASE WHEN e IS NOT NULL THEN s END) AS s, MAX(e) AS e, COUNT(*) AS n, COALESCE(SUM(e IS NULL), 0) AS unknown_intervals FROM _usage_selected WHERE operation_kind = 'model_request'")
        .fetch_one(&mut *connection).await.map_err(super::database_error)?;
    let request_start: Option<i64> = request.get("s");
    let request_end: Option<i64> = request.get("e");
    let request_ms = match (request_start, request_end) {
        (Some(s), Some(e)) => e.checked_sub(s).ok_or(UsageStoreError::AggregateOverflow)?,
        _ => 0,
    };
    let unknown = if request.get::<i64,_>("n") == 0 {1} else {request.get("unknown_intervals")};
    timing.request_to_delivery_wall = duration(request_ms, unknown)?;
    let rows = sqlx::query("SELECT COALESCE(terminal_status, 'unknown') AS outcome, COUNT(*) AS n, COALESCE(SUM(e - s), 0) AS measured_ms, SUM(e IS NULL) AS unknown_intervals FROM _usage_selected WHERE operation_kind IN ('local_tool','hosted_tool','activity_control') GROUP BY outcome ORDER BY outcome")
        .fetch_all(&mut *connection).await.map_err(super::database_error)?;
    let mut tools = ToolMetrics { count:0, duration:duration(/*milliseconds*/ 0, /*unknown*/ 0)?, outcomes:Vec::new(), duration_basis:"sum of clipped UTC wall intervals; lifecycle facts retain full monotonic duration" };
    for row in rows {
        let count = u64::try_from(row.get::<i64,_>("n")).map_err(|_| UsageStoreError::AggregateOverflow)?;
        let value = duration(row.try_get("measured_ms").map_err(|_| UsageStoreError::AggregateOverflow)?, row.get("unknown_intervals"))?;
        tools.count = tools.count.checked_add(count).ok_or(UsageStoreError::AggregateOverflow)?;
        tools.duration.measured_ns = tools.duration.measured_ns.checked_add(value.measured_ns).ok_or(UsageStoreError::AggregateOverflow)?;
        tools.duration.unknown_intervals = tools.duration.unknown_intervals.checked_add(value.unknown_intervals).ok_or(UsageStoreError::AggregateOverflow)?;
        tools.outcomes.push(ToolOutcomeCount {outcome:row.get("outcome"),count});
    }
    tools.duration.exact_ns = (tools.duration.unknown_intervals == 0).then_some(tools.duration.measured_ns);
    Ok((timing, tools))
}

fn duration(milliseconds: i64, unknown: i64) -> Result<DurationAggregate, UsageStoreError> {
    let measured_ns = u64::try_from(milliseconds).ok().and_then(|ms| ms.checked_mul(1_000_000)).ok_or(UsageStoreError::AggregateOverflow)?;
    let unknown_intervals = u64::try_from(unknown).map_err(|_| UsageStoreError::AggregateOverflow)?;
    Ok(DurationAggregate { measured_ns, exact_ns:(unknown_intervals == 0).then_some(measured_ns), unknown_intervals })
}
