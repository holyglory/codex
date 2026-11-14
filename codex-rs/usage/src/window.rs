//! One prepared, immutable report view shared by a finite request.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::BoundOperationEffort;
use crate::BoundWorkEffort;
use crate::OperationPage;
use crate::OperationSelection;
use crate::RecordCursor;
use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::report_query::ReportQuery;

/// Reuse one canonical selection for a pinned reader, range, and scope.
/// This view owns only temporary query tables; it never publishes accounting.
pub struct UsageWindow<'a> {
    reader: &'a UsageReader,
    range: TimeRange,
    filter: &'a UsageFilter,
    query: ReportQuery,
}

impl UsageReader {
    /// Prepare one immutable report view for related finite-window reads.
    pub fn prepare_window<'a>(
        &'a self,
        range: TimeRange,
        filter: &'a UsageFilter,
    ) -> Result<UsageWindow<'a>> {
        Ok(UsageWindow {
            reader: self,
            range,
            filter,
            query: ReportQuery::new(self, range, filter)?,
        })
    }
}

impl UsageWindow<'_> {
    /// Read exact totals and canonical counts from this prepared selection.
    pub fn summary(&self) -> Result<Value> {
        self.reader
            .summary_from_query(self.range, self.filter, &self.query)
    }

    /// Read performance and coverage without preparing another selection.
    pub fn performance_review(&self) -> Result<Value> {
        self.reader
            .review_from_query(self.range, self.filter, &self.query)
    }

    /// Read sparse work assignments, preserving exceptional declarations.
    pub fn bound_work_effort(
        &self,
        exceptional: &BTreeSet<String>,
    ) -> Result<Vec<BoundWorkEffort>> {
        crate::work_binding_report::bound_work_effort_from_query(
            &self.query,
            self.range,
            exceptional,
        )
    }

    /// Read evidence for the same bounded exceptional operation identities.
    pub fn bound_operation_effort(
        &self,
        operation_ids: &BTreeSet<String>,
    ) -> Result<Vec<BoundOperationEffort>> {
        self.reader
            .bound_effort_from_query(self.range, operation_ids, &self.query)
    }

    /// Page canonical operations from this exact prepared selection.
    pub fn operation_page(
        &self,
        selection: OperationSelection,
        after: Option<&RecordCursor>,
        limit: usize,
    ) -> Result<OperationPage> {
        self.reader
            .operation_page_from_query(selection, after, limit, &self.query)
    }
}
