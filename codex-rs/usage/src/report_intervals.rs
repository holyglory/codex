//! Disk-backed background intervals, sorted by DuckDB and consumed one batch at a time.
use std::collections::BTreeMap;
use std::sync::Arc;

use duckdb::Connection;
use duckdb::arrow::array::Int64Array;
use duckdb::arrow::array::StringArray;
use duckdb::arrow::datatypes::DataType;
use duckdb::arrow::datatypes::Field;
use duckdb::arrow::datatypes::Schema;
use serde::Deserialize;

use super::IntervalMetrics;
use super::sql::interval_rows_query;
use crate::Result;
use crate::global_review_cancel::GlobalReviewCancellation;
use crate::review_state::invalid;
use crate::review_storage::query_error;
use crate::review_storage::query_rows;

pub(crate) fn background_grouped_interval_metrics(
    connection: &Connection,
    operations: &str,
    start_ms: i64,
    end_ms: i64,
    cancellation: Option<&GlobalReviewCancellation>,
) -> Result<BTreeMap<String, IntervalMetrics>> {
    let check = || cancellation.map_or(Ok(()), GlobalReviewCancellation::check);
    check()?;
    let projection = interval_rows_query(operations, start_ms, end_ms);
    connection.execute_batch(&format!(
        "DROP TABLE IF EXISTS background_interval_spans; DROP TABLE IF EXISTS background_intervals; \
         CREATE TABLE background_intervals AS {projection}"
    )).map_err(|error| query_error_at("persist_intervals", error))?;
    check()?;
    #[derive(Deserialize)]
    struct Group {
        timing_group: String,
        #[serde(flatten)]
        metrics: IntervalMetrics,
    }
    let summary = "WITH summaries AS (
        SELECT timing_group,sum(observation_weight)::UBIGINT operations,
            COALESCE(sum(observation_weight) FILTER (WHERE kind<>'wait'),0)::UBIGINT execution_observations,
            COALESCE(sum(observation_weight) FILTER (WHERE kind='wait'),0)::UBIGINT wait_observations,
            min(started_at_ms) FILTER (WHERE reason IS NULL) first_started_at_ms,
            max(ended_at_ms) FILTER (WHERE reason IS NULL) last_ended_at_ms,
            COALESCE(sum(observation_weight) FILTER (WHERE kind='wait' AND reason IS NOT NULL),0)::UBIGINT wait_unknown,
            COALESCE(sum(observation_weight) FILTER (WHERE kind<>'wait' AND (reason IS NOT NULL OR
                (interval_owner IS NULL AND clipped_start<clipped_end))),0)::UBIGINT elapsed_unknown,
            COALESCE(sum(observation_weight) FILTER (WHERE kind<>'wait' AND (reason IS NOT NULL OR
                (interval_owner IS NULL AND clipped_start<clipped_end))),0)::UBIGINT active_unknown
        FROM background_intervals GROUP BY timing_group
        ), unknowns AS (
            SELECT timing_group,kind || ':' || COALESCE(reason,'missing_active_owner') reason_key,
                sum(observation_weight)::UBIGINT observations
            FROM background_intervals WHERE reason IS NOT NULL OR
                (kind<>'wait' AND interval_owner IS NULL AND clipped_start<clipped_end) GROUP BY ALL
        ), reasons AS (
            SELECT timing_group,json_group_object(reason_key,observations) unknown_reasons
                FROM unknowns GROUP BY timing_group
        ) SELECT summaries.*,0::UBIGINT active_ms,0::UBIGINT elapsed_ms,0::UBIGINT wait_ms,
            COALESCE(unknown_reasons,'{}'::JSON) unknown_reasons
        FROM summaries LEFT JOIN reasons USING(timing_group)";
    let mut metrics = query_rows::<Group>(connection, summary, /*max_rows*/ 16_643)?
        .into_iter()
        .map(|row| (row.timing_group, row.metrics))
        .collect::<BTreeMap<_, _>>();
    check()?;
    connection.execute_batch(
        "CREATE TABLE background_interval_spans AS
            SELECT timing_group,'wait' metric,'' interval_owner,clipped_start::BIGINT span_start,clipped_end::BIGINT span_end
                FROM background_intervals WHERE reason IS NULL AND clipped_start<clipped_end AND kind='wait'
            UNION ALL
            SELECT timing_group,'elapsed','',clipped_start::BIGINT,clipped_end::BIGINT
                FROM background_intervals WHERE reason IS NULL AND clipped_start<clipped_end AND kind<>'wait'
            UNION ALL
            SELECT timing_group,'active',interval_owner,clipped_start::BIGINT,clipped_end::BIGINT
                FROM background_intervals WHERE reason IS NULL AND clipped_start<clipped_end AND kind<>'wait'
                    AND interval_owner IS NOT NULL"
    ).map_err(|error| query_error_at("persist_spans", error))?;
    check()?;
    let expected: u64 = connection
        .query_row(
            "SELECT count(*)::UBIGINT FROM background_interval_spans",
            [],
            |row| row.get(0),
        )
        .map_err(|error| query_error_at("span_count", error))?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("timing_group", DataType::Utf8, /*nullable*/ false),
        Field::new("metric", DataType::Utf8, /*nullable*/ false),
        Field::new("interval_owner", DataType::Utf8, /*nullable*/ false),
        Field::new("span_start", DataType::Int64, /*nullable*/ false),
        Field::new("span_end", DataType::Int64, /*nullable*/ false),
    ]));
    let mut statement = connection.prepare(
        "SELECT timing_group,metric,interval_owner,span_start,span_end FROM background_interval_spans \
         ORDER BY timing_group,metric,interval_owner,span_start,span_end"
    ).map_err(|error| query_error_at("sort_prepare", error))?;
    check()?;
    let stream = statement
        .stream_arrow([], schema)
        .map_err(|error| query_error_at("sort_start", error))?;
    let mut current: Option<(String, String, String, i64)> = None;
    let mut consumed = 0_u64;
    for batch in stream {
        let group = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| invalid("invalid background interval group column"))?;
        let metric = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| invalid("invalid background interval metric column"))?;
        let owner = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| invalid("invalid background interval owner column"))?;
        let start = batch
            .column(3)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| invalid("invalid background interval start column"))?;
        let end = batch
            .column(4)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| invalid("invalid background interval end column"))?;
        for index in 0..batch.num_rows() {
            if consumed.is_multiple_of(4096) {
                check()?;
            }
            let (group, metric, owner, start, end) = (
                group.value(index),
                metric.value(index),
                owner.value(index),
                start.value(index),
                end.value(index),
            );
            let covered = if let Some((prior_group, prior_metric, prior_owner, prior_end)) =
                &current
                && (
                    prior_group.as_str(),
                    prior_metric.as_str(),
                    prior_owner.as_str(),
                ) == (group, metric, owner)
            {
                start.max(*prior_end)
            } else {
                start
            };
            let added = u64::try_from((i128::from(end) - i128::from(covered)).max(0))
                .map_err(|_| invalid("background interval duration overflow"))?;
            let row = metrics
                .get_mut(group)
                .ok_or_else(|| invalid("background interval group missing its summary"))?;
            let measured = match metric {
                "active" => &mut row.active_ms,
                "elapsed" => &mut row.elapsed_ms,
                "wait" => &mut row.wait_ms,
                _ => return Err(invalid("invalid background interval metric")),
            };
            *measured = measured
                .checked_add(added)
                .ok_or_else(|| invalid("background interval duration overflow"))?;
            if let Some((prior_group, prior_metric, prior_owner, prior_end)) = &mut current
                && (
                    prior_group.as_str(),
                    prior_metric.as_str(),
                    prior_owner.as_str(),
                ) == (group, metric, owner)
            {
                *prior_end = (*prior_end).max(end);
            } else {
                current = Some((group.to_owned(), metric.to_owned(), owner.to_owned(), end));
            }
            consumed = consumed
                .checked_add(1)
                .ok_or_else(|| invalid("background interval row count overflow"))?;
        }
    }
    check()?;
    // DuckDB's Arrow stream exposes end-of-stream without a fetch error. An
    // interrupted or incomplete result must never become a completed union.
    if consumed != expected {
        return Err(invalid(
            "background interval stream ended before its exact source count",
        ));
    }
    Ok(metrics)
}

fn query_error_at(stage: &str, error: duckdb::Error) -> crate::StorageError {
    crate::StorageError::Columnar(format!(
        "background interval stage {stage}: {}",
        query_error(error)
    ))
}
