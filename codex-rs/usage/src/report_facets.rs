//! Small operation breakdowns with exact totals for every combined group.
use duckdb::Connection;
use serde_json::Value;

use crate::Result;
use crate::review_storage::query_rows;

const LEADING_GROUPS: usize = 16;

/// Return a deterministic prefix and a marked remainder. Aggregate every
/// omitted group's measurements before applying the output row bound.
pub(crate) fn operation_facet(
    connection: &Connection,
    groups: &str,
    dimensions: &[&str],
) -> Result<Vec<Value>> {
    let names = dimensions.join(", ");
    let remainder = dimensions
        .iter()
        .map(|name| format!("NULL::VARCHAR {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    query_rows(
        connection,
        &format!(
            "WITH grouped AS ({groups}), ranked AS \
             (SELECT *, row_number() OVER (ORDER BY {names}) group_rank FROM grouped), \
             facets AS (SELECT {names}, count, \"measuredIntervalSumMs\", \"unknownIntervals\", \
                 1::UBIGINT \"mergedGroups\", false remainder FROM ranked WHERE group_rank <= {LEADING_GROUPS} \
             UNION ALL SELECT {remainder}, sum(count)::UBIGINT count, \
                 sum(\"measuredIntervalSumMs\")::UBIGINT \"measuredIntervalSumMs\", \
                 sum(\"unknownIntervals\")::UBIGINT \"unknownIntervals\", \
                 count(*)::UBIGINT \"mergedGroups\", true remainder \
                 FROM ranked WHERE group_rank > {LEADING_GROUPS} HAVING count(*) > 0) \
             SELECT * FROM facets ORDER BY remainder, {names}"
        ),
        LEADING_GROUPS + 1,
    )
}
