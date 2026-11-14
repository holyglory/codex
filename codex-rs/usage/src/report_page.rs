use serde::Deserialize;
use serde::Serialize;

use crate::RecordCursor;
use crate::Result;
use crate::ReviewOperation;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::quoted;
use crate::report_query::ReportQuery;
use crate::review_storage::query_rows;

/// Factual operation families exposed by the list and detail actions.
#[derive(Clone, Copy, Debug)]
pub enum OperationSelection {
    /// All measured runtime operations.
    All,
    /// Tool invocations, one latest row per invocation.
    Tools,
    /// Model requests, excluding connection warmups.
    ModelRequests,
    /// Operations carrying an explicit phase/activity classification.
    Classified,
}

/// One bounded page, with a keyset continuation rather than an offset scan.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationPage {
    /// Canonical operations with any late taxonomy correction applied.
    pub data: Vec<ReviewOperation>,
    /// Last returned operation; absent at the end of the pinned query.
    pub next_cursor: Option<RecordCursor>,
}

impl UsageReader {
    /// List canonical operations through the same indexed view as reviews.
    pub fn operation_page(
        &self,
        range: TimeRange,
        filter: &UsageFilter,
        selection: OperationSelection,
        after: Option<&RecordCursor>,
        limit: usize,
    ) -> Result<OperationPage> {
        let query = ReportQuery::new(self, range, filter)?;
        self.operation_page_from_query(selection, after, limit, &query)
    }

    pub(crate) fn operation_page_from_query(
        &self,
        selection: OperationSelection,
        after: Option<&RecordCursor>,
        limit: usize,
        query: &ReportQuery,
    ) -> Result<OperationPage> {
        let limit = limit.clamp(1, 50);
        let mut predicates = vec![match selection {
            OperationSelection::All => "true".to_string(),
            OperationSelection::Tools => "kind = 'tool'".to_string(),
            OperationSelection::ModelRequests => "kind = 'model_request'".to_string(),
            OperationSelection::Classified => "activity IS NOT NULL".to_string(),
        }];
        if let Some(after) = after {
            predicates.push(format!(
                "(COALESCE(started_at_ms, observed_at_ms),operation_id) > ({},{})",
                after.observed_at_ms,
                quoted(&after.id)
            ));
        }
        let mut data = query_rows::<ReviewOperation>(
            &query.connection,
            &format!(
                "SELECT * FROM ({}) WHERE {} ORDER BY COALESCE(started_at_ms,observed_at_ms),operation_id LIMIT {}",
                query.operations,
                predicates.join(" AND "),
                limit + 1
            ),
            limit + 1,
        )?;
        let has_more = data.len() > limit;
        data.truncate(limit);
        let next_cursor = if has_more {
            data.last().map(|row| {
                RecordCursor::from_millis(
                    row.started_at_ms.unwrap_or(row.observed_at_ms),
                    row.operation_id.clone(),
                )
            })
        } else {
            None
        };
        Ok(OperationPage { data, next_cursor })
    }
}
