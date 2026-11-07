//! Mergeable operation metadata. Publication replaces only a compact summary
//! sidecar; foreground single-dimension summaries read at most one indexed row.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use arrow_schema::DataType;
use duckdb::Connection;
use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::UsageOutcome;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_projection::load_review_operations;
use crate::review_state::ReviewClassification;
use crate::review_state::ReviewOperation;
use crate::review_state::ReviewOperationKind;
use crate::review_state::invalid;
use crate::review_storage::parquet_source;
use crate::review_storage::query_error;
use crate::review_storage::query_rows;
use crate::review_storage::quoted;
use crate::review_storage::segment_path;
use crate::review_storage::write_rows;
use crate::storage_file;

macro_rules! counts {
    ($($field:ident),* $(,)?) => {
        /// Exact counts of current canonical operation states. These are not
        /// token observations, retries inferred from tokens, or wall-clock time.
        #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
        pub struct ReviewCounts { $(pub $field: u64),* }
        const FIELDS: &[&str] = &[$(stringify!($field)),*];
        impl ReviewCounts {
            /// Combine disjoint operation sets, rejecting overflow.
            pub fn checked_add(&self, other: &Self) -> Result<Self> {
                Ok(Self { $($field: self.$field.checked_add(other.$field)
                    .ok_or_else(|| invalid("operation summary overflow"))?),* })
            }
            /// Remove a known subset before replacing bounded hot-tail states.
            pub fn checked_sub(&self, other: &Self) -> Result<Self> {
                Ok(Self { $($field: self.$field.checked_sub(other.$field)
                    .ok_or_else(|| invalid("operation summary underflow"))?),* })
            }
            fn values(&self) -> [u64; FIELDS.len()] { [$(self.$field),*] }
        }
    };
}

counts![
    operations,
    model_requests,
    tools,
    warmups,
    unfinished,
    unknown_intervals,
    unclassified,
    missing_repository,
    tool_completed,
    tool_failed,
    tool_blocked,
    tool_cancelled,
    tool_incomplete,
];

impl ReviewCounts {
    /// Count one operation whose phase/activity already include its latest
    /// explicit correction. Terminal facts count even when no start was seen.
    pub fn for_operation(operation: &ReviewOperation) -> Self {
        if operation.kind == ReviewOperationKind::Wait {
            return Self::default();
        }
        let tool = operation.kind == ReviewOperationKind::Tool;
        Self {
            operations: 1,
            model_requests: u64::from(operation.kind == ReviewOperationKind::ModelRequest),
            tools: u64::from(tool),
            warmups: u64::from(operation.kind == ReviewOperationKind::ModelWarmup),
            unfinished: u64::from(!operation.terminal_observed),
            unknown_intervals: u64::from(
                operation.started_at_ms.is_none() || operation.ended_at_ms.is_none(),
            ),
            unclassified: u64::from(operation.phase.is_none() || operation.activity.is_none()),
            missing_repository: u64::from(operation.repository_id.is_none()),
            tool_completed: u64::from(tool && operation.outcome == Some(UsageOutcome::Completed)),
            tool_failed: u64::from(tool && operation.outcome == Some(UsageOutcome::Failed)),
            tool_blocked: u64::from(tool && operation.outcome == Some(UsageOutcome::Blocked)),
            tool_cancelled: u64::from(tool && operation.outcome == Some(UsageOutcome::Cancelled)),
            tool_incomplete: u64::from(tool && operation.outcome == Some(UsageOutcome::Incomplete)),
        }
    }
}

/// Dimensions with an exact all-time summary row; intersections use indexed
/// report queries rather than pretending independently grouped counts intersect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewCountScope {
    Global,
    Account(String),
    Repository(String),
    Thread(String),
}

#[derive(Deserialize)]
struct CountsRow {
    dimension_kind: String,
    dimension_id: String,
    #[serde(flatten)]
    counts: ReviewCounts,
}

/// Read one pinned metadata row. Completeness and frontier are supplied by the
/// same descriptor; callers must retain its complete_from_sequence/watermark.
pub fn read_review_counts(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
    scope: &ReviewCountScope,
) -> Result<ReviewCounts> {
    let Some(file) = &descriptor.counts_file else {
        if !descriptor.partitions.is_empty() {
            return Err(invalid("operation counts have not been published"));
        }
        return Ok(ReviewCounts::default());
    };
    let (kind, id) = match scope {
        ReviewCountScope::Global => ("all", ""),
        ReviewCountScope::Account(id) => ("account", id.as_str()),
        ReviewCountScope::Repository(id) => ("repository", id.as_str()),
        ReviewCountScope::Thread(id) => ("thread", id.as_str()),
    };
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let source = parquet_source(root, std::slice::from_ref(file))?;
    let rows = query_rows::<CountsRow>(
        &connection,
        &format!(
            "SELECT * FROM {source} WHERE dimension_kind = {} AND dimension_id = {}",
            quoted(kind),
            quoted(id)
        ),
        /*max_rows*/ 1,
    )?;
    Ok(rows
        .into_iter()
        .next()
        .map(|row| row.counts)
        .unwrap_or_default())
}

/// Read the compact count sidecar once for publisher metadata caching.
pub(crate) fn read_review_counts_map(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
) -> Result<BTreeMap<String, ReviewCounts>> {
    let Some(file) = &descriptor.counts_file else {
        if !descriptor.partitions.is_empty() {
            return Err(invalid("operation counts have not been published"));
        }
        return Ok(BTreeMap::new());
    };
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let source = parquet_source(root, std::slice::from_ref(file))?;
    let rows: Vec<CountsRow> =
        query_rows(&connection, &format!("SELECT * FROM {source}"), 100_000)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let key = match (row.dimension_kind.as_str(), row.dimension_id.as_str()) {
                ("all", "") => "all".to_string(),
                (kind, id) => format!("{kind}:{id}"),
            };
            (key, row.counts)
        })
        .collect())
}

pub(crate) struct ReviewCountsUpdate<'a> {
    root: &'a Path,
    previous: ReviewProjectionDescriptor,
    connection: &'a Connection,
    classifications: BTreeMap<String, ReviewClassification>,
    updated: BTreeSet<String>,
    deltas: BTreeMap<(String, String), Vec<i64>>,
}

impl<'a> ReviewCountsUpdate<'a> {
    pub(crate) fn new(
        root: &'a Path,
        previous: ReviewProjectionDescriptor,
        classifications: &[ReviewClassification],
        connection: &'a Connection,
    ) -> Self {
        let mut latest = BTreeMap::<String, ReviewClassification>::new();
        for classification in classifications {
            if latest
                .get(&classification.operation_id)
                .is_none_or(|old| old.source_sequence < classification.source_sequence)
            {
                latest.insert(classification.operation_id.clone(), classification.clone());
            }
        }
        Self {
            root,
            previous,
            connection,
            classifications: latest,
            updated: BTreeSet::new(),
            deltas: BTreeMap::new(),
        }
    }

    pub(crate) fn replace_operations(
        &mut self,
        before: &[ReviewOperation],
        after: &[ReviewOperation],
    ) -> Result<()> {
        let ids = before
            .iter()
            .chain(after)
            .map(|row| row.operation_id.clone())
            .collect::<BTreeSet<_>>();
        let classifications = self.previous_classifications(&ids)?;
        for state in before {
            self.delta(
                state,
                classifications.get(&state.operation_id),
                /*sign*/ -1,
            )?;
        }
        for state in after {
            let old = classifications.get(&state.operation_id);
            let latest = self
                .classifications
                .get(&state.operation_id)
                .filter(|new| old.is_none_or(|old| new.source_sequence > old.source_sequence))
                .or(old)
                .cloned();
            self.delta(state, latest.as_ref(), /*sign*/ 1)?;
        }
        self.updated.extend(
            ids.into_iter()
                .filter(|id| self.classifications.contains_key(id)),
        );
        self.deltas
            .retain(|_, values| values.iter().any(|value| *value != 0));
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        stamp: &str,
        cached_counts: &mut BTreeMap<String, ReviewCounts>,
    ) -> Result<Option<String>> {
        let ids = self
            .classifications
            .keys()
            .filter(|id| !self.updated.contains(*id))
            .cloned()
            .collect::<BTreeSet<_>>();
        let states = load_review_operations(self.root, &self.previous, &ids, self.connection)?;
        self.replace_operations(&states, &states)?;
        self.deltas
            .retain(|_, values| values.iter().any(|value| *value != 0));
        if self.deltas.is_empty() {
            return Ok(self.previous.counts_file);
        }
        let mut columns = vec![
            ("dimension_kind", DataType::Utf8),
            ("dimension_id", DataType::Utf8),
        ];
        columns.extend(FIELDS.iter().map(|name| (*name, DataType::Int64)));
        let mut rows = Vec::new();
        for ((kind, id), values) in self.deltas {
            let key = if kind == "all" {
                "all".to_string()
            } else {
                format!("{kind}:{id}")
            };
            let previous = cached_counts.entry(key).or_default();
            let mut adjusted = serde_json::Map::new();
            for ((field, value), delta) in FIELDS.iter().zip(previous.values()).zip(&values) {
                let value = if *delta < 0 {
                    value.checked_sub(delta.unsigned_abs())
                } else {
                    value.checked_add(*delta as u64)
                }
                .ok_or_else(|| invalid("cached operation summary delta overflow or underflow"))?;
                adjusted.insert((*field).into(), value.into());
            }
            *previous = serde_json::from_value(serde_json::Value::Object(adjusted))?;
            let mut row = serde_json::Map::new();
            row.insert("dimension_kind".into(), kind.into());
            row.insert("dimension_id".into(), id.into());
            for (name, value) in FIELDS.iter().zip(values) {
                row.insert((*name).into(), value.into());
            }
            rows.push(serde_json::Value::Object(row));
        }
        let delta_file = format!("review-count-delta-{stamp}.parquet");
        write_rows(self.root, &delta_file, &rows, &columns)?;
        let additions = parquet_source(self.root, std::slice::from_ref(&delta_file))?;
        let source = if let Some(previous) = &self.previous.counts_file {
            format!(
                "SELECT * FROM {} UNION ALL SELECT * FROM {additions}",
                parquet_source(self.root, std::slice::from_ref(previous))?
            )
        } else {
            format!("SELECT * FROM {additions}")
        };
        let totals = FIELDS
            .iter()
            .map(|field| format!("SUM({field})::UBIGINT AS {field}"))
            .collect::<Vec<_>>()
            .join(", ");
        let file = format!("review-counts-{stamp}.parquet");
        let path = segment_path(self.root, &file)?;
        self.connection.execute_batch(&format!(
            "COPY (SELECT dimension_kind, dimension_id, {totals} FROM ({source}) GROUP BY dimension_kind, dimension_id ORDER BY dimension_kind, dimension_id) TO {} (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 2048)",
            quoted(&path.to_string_lossy()))).map_err(query_error)?;
        storage_file::finish_generated(&path)?;
        std::fs::remove_file(segment_path(self.root, &delta_file)?)?;
        Ok(Some(file))
    }

    fn previous_classifications(
        &self,
        ids: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, ReviewClassification>> {
        if ids.is_empty() || self.previous.classification_files.is_empty() {
            return Ok(BTreeMap::new());
        }
        let source = parquet_source(self.root, &self.previous.classification_files)?;
        let candidates = ids
            .iter()
            .map(|id| quoted(id))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = query_rows::<ReviewClassification>(
            self.connection,
            &format!(
                "SELECT * FROM {source} WHERE operation_id IN ({candidates}) QUALIFY row_number() OVER (PARTITION BY operation_id ORDER BY source_sequence DESC) = 1"
            ),
            ids.len(),
        )?;
        Ok(rows
            .into_iter()
            .map(|row| (row.operation_id.clone(), row))
            .collect())
    }

    fn delta(
        &mut self,
        state: &ReviewOperation,
        classification: Option<&ReviewClassification>,
        sign: i64,
    ) -> Result<()> {
        if state.kind == ReviewOperationKind::Wait {
            return Ok(());
        }
        let mut counts = ReviewCounts::for_operation(state);
        if let Some(classification) = classification {
            counts.unclassified = u64::from(
                classification
                    .phase
                    .as_ref()
                    .or(state.phase.as_ref())
                    .is_none()
                    || classification
                        .activity
                        .as_ref()
                        .or(state.activity.as_ref())
                        .is_none(),
            );
        }
        for (kind, id) in [
            ("all", Some("")),
            ("account", state.account_id.as_deref()),
            ("thread", state.thread_id.as_deref()),
            ("repository", state.repository_id.as_deref()),
        ] {
            if let Some(id) = id {
                let values = self
                    .deltas
                    .entry((kind.to_string(), id.to_string()))
                    .or_insert_with(|| vec![0; FIELDS.len()]);
                for (value, count) in values.iter_mut().zip(counts.values()) {
                    *value = value
                        .checked_add((count as i64) * sign)
                        .ok_or_else(|| invalid("operation summary delta overflow"))?;
                }
            }
        }
        Ok(())
    }
}
