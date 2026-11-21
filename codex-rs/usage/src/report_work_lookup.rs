//! Route explicit late-token operation IDs in bounded batches, then read each
//! required state partition once without round-tripping its JSON through Rust.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use duckdb::Connection;

use crate::Result;
use crate::operation_id_index;
use crate::reader_query::parquet_source;
use crate::reader_query::query_error;
use crate::report_query::Materialization;
use crate::report_query::sql_columns;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_projection::find_addresses;
use crate::review_state::REVIEW_PARTITION_SIZE;
use crate::review_state::invalid;
use crate::review_storage::OPERATION_COLUMNS;

pub(super) fn populate(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
    connection: &Connection,
    materialization: Materialization,
    cancellation: Option<&crate::global_review_cancel::GlobalReviewCancellation>,
) -> Result<()> {
    let check = || {
        cancellation.map_or(
            Ok(()),
            crate::global_review_cancel::GlobalReviewCancellation::check,
        )
    };
    let table = materialization.table();
    check()?;
    connection
        .execute_batch(&format!(
            "CREATE {table} work_operation_references AS \
         SELECT operation_id, row_number() OVER ()::UBIGINT reference_position \
         FROM (SELECT DISTINCT c.operation_id FROM selected_contributions c \
         WHERE c.operation_id IS NOT NULL AND c.work_native_project_id IS NULL \
         AND ({}) AND NOT EXISTS \
         (SELECT 1 FROM selected_operations o WHERE o.operation_id=c.operation_id));",
            crate::review_aggregate::TOKEN_PREDICATE,
        ))
        .map_err(query_error)?;
    check()?;
    connection
        .execute_batch(&format!(
            "CREATE {table} work_operation_addresses \
        (operation_id VARCHAR PRIMARY KEY, first_sequence UBIGINT NOT NULL)"
        ))
        .map_err(query_error)?;
    let partitions = descriptor
        .partitions
        .iter()
        .cloned()
        .map(|partition| (partition.partition, partition))
        .collect::<BTreeMap<_, _>>();
    let index = operation_id_index::load(root, descriptor)?;
    let mut files = BTreeSet::new();
    let mut after = 0_u64;
    loop {
        check()?;
        let end = after
            .checked_add(256)
            .ok_or_else(|| invalid("operation reference range overflow"))?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT operation_id FROM work_operation_references \
             WHERE reference_position > {after} AND reference_position <= {end}"
            ))
            .map_err(query_error)?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(query_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)?;
        if ids.is_empty() {
            break;
        }
        let last_batch = ids.len() < 256;
        check()?;
        let addresses = find_addresses(root, &partitions, ids, index.as_ref(), connection)?;
        if !addresses.is_empty() {
            check()?;
            let mut appender = connection
                .appender("work_operation_addresses")
                .map_err(query_error)?;
            for (id, first) in addresses {
                files.insert(
                    partitions[&((first - 1) / REVIEW_PARTITION_SIZE)]
                        .file
                        .clone(),
                );
                appender
                    .append_row(duckdb::params![id, first])
                    .map_err(query_error)?;
            }
            appender.flush().map_err(query_error)?;
        }
        if last_batch {
            break;
        }
        after = end;
    }
    if files.is_empty() {
        return Ok(());
    }
    let source = parquet_source(
        &root.join("segments"),
        &files.into_iter().collect::<Vec<_>>(),
        &sql_columns(OPERATION_COLUMNS),
    )?;
    check()?;
    connection
        .execute_batch(&format!(
            "INSERT INTO work_operation_lookup SELECT o.* FROM ({source}) o \
         JOIN work_operation_addresses a USING (operation_id)"
        ))
        .map_err(query_error)?;
    check()?;
    let (rows, unique, mismatch, expected): (u64, u64, bool, u64) = connection
        .query_row(
            "SELECT count(*), count(DISTINCT o.operation_id), \
         COALESCE(bool_or(o.first_sequence IS DISTINCT FROM a.first_sequence),false), \
         (SELECT count(*) FROM work_operation_addresses) \
         FROM work_operation_lookup o JOIN work_operation_addresses a USING (operation_id)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(query_error)?;
    if mismatch || rows != unique {
        return Err(invalid(
            "operation lookup disagrees with its current id index",
        ));
    }
    if rows != expected {
        return Err(invalid("operation lookup is missing indexed state"));
    }
    Ok(())
}
