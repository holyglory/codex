//! Interval membership is separate from operation rows, so selection never has
//! to enumerate operations belonging to unrelated repositories or time windows.
use std::collections::BTreeMap;
use std::path::Path;

use duckdb::Connection;
use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_storage::MEMBERSHIP_COLUMNS;
use crate::review_storage::parquet_source;
use crate::review_storage::query_error;
use crate::review_storage::quoted;
use crate::review_storage::write_rows;

/// Conservative exact bounds; missing endpoints can never exclude a window.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewIntervalBounds {
    pub min_started_at_ms: Option<i64>,
    pub max_ended_at_ms: Option<i64>,
    pub unknown_start: bool,
    pub unknown_end: bool,
}

impl ReviewIntervalBounds {
    fn include(&mut self, start: Option<i64>, end: Option<i64>) {
        if let Some(start) = start {
            self.min_started_at_ms =
                Some(self.min_started_at_ms.map_or(start, |old| old.min(start)));
        } else {
            self.unknown_start = true;
        }
        if let Some(end) = end {
            self.max_ended_at_ms = Some(self.max_ended_at_ms.map_or(end, |old| old.max(end)));
        } else {
            self.unknown_end = true;
        }
    }

    pub(crate) fn overlaps(&self, range: TimeRange) -> bool {
        range.start_ms() < range.end_ms()
            && (self.unknown_start
                || self
                    .min_started_at_ms
                    .is_none_or(|start| start < range.end_ms()))
            && (self.unknown_end
                || self
                    .max_ended_at_ms
                    .is_none_or(|end| end >= range.start_ms()))
    }
}

pub(crate) struct IndexedInterval<'a> {
    pub account_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub repository_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

#[derive(Serialize)]
struct Membership {
    dimension_kind: String,
    dimension_id: String,
    segment_file: String,
    #[serde(flatten)]
    bounds: ReviewIntervalBounds,
}

pub(crate) fn write_membership<'a>(
    root: &Path,
    file: &str,
    segment_file: &str,
    intervals: impl IntoIterator<Item = IndexedInterval<'a>>,
) -> Result<ReviewIntervalBounds> {
    let mut memberships = BTreeMap::<(String, String), ReviewIntervalBounds>::new();
    for interval in intervals {
        for (kind, id) in [
            ("all", Some("")),
            ("account", interval.account_id),
            ("thread", interval.thread_id),
            ("repository", interval.repository_id),
            ("model", interval.model),
        ] {
            if let Some(id) = id {
                memberships
                    .entry((kind.to_string(), id.to_string()))
                    .or_default()
                    .include(interval.start, interval.end);
            }
        }
    }
    let bounds = memberships
        .get(&("all".to_string(), String::new()))
        .cloned()
        .unwrap_or_default();
    let rows = memberships
        .into_iter()
        .map(|((dimension_kind, dimension_id), bounds)| Membership {
            dimension_kind,
            dimension_id,
            segment_file: segment_file.to_string(),
            bounds,
        })
        .collect::<Vec<_>>();
    write_rows(root, file, &rows, MEMBERSHIP_COLUMNS)?;
    Ok(bounds)
}

/// Select current operation partitions using only descriptor and membership
/// metadata. Returned names are relative to `root/segments`; callers still
/// apply exact row predicates and join late classification by operation id.
pub fn select_review_files(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<Vec<String>> {
    let routed = [
        ("account", filter.account_id.as_deref()),
        ("repository", filter.repository_id.as_deref()),
        ("model", filter.model.as_deref()),
    ]
    .into_iter()
    .find_map(|(kind, id)| {
        id.and_then(|id| descriptor.dimension_partitions.get(&format!("{kind}:{id}")))
    });
    select_memberships(
        root,
        descriptor.partitions.iter().filter_map(|partition| {
            if routed.is_some_and(|partitions| !partitions.contains(&partition.partition)) {
                return None;
            }
            partition
                .bounds
                .overlaps(range)
                .then_some((partition.file.as_str(), partition.membership_file.as_str()))
        }),
        range,
        filter,
        descriptor.membership_index_file.as_deref(),
    )
}

/// Select canonical observation fragments by their observation window and
/// dimension index, without opening operation or token data. To attribute all
/// observations of selected operations regardless of observation time, callers
/// may instead join descriptor.contribution_fragments by operation/response id.
pub fn select_review_contribution_files(
    root: &Path,
    descriptor: &ReviewProjectionDescriptor,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<Vec<String>> {
    select_memberships(
        root,
        descriptor
            .contribution_fragments
            .iter()
            .filter_map(|fragment| {
                (range.start_ms() < range.end_ms()
                    && fragment.min_observed_at_ms < range.end_ms()
                    && fragment.max_observed_at_ms >= range.start_ms())
                .then_some((fragment.file.as_str(), fragment.membership_file.as_str()))
            }),
        range,
        filter,
        None,
    )
}

fn select_memberships<'a>(
    root: &Path,
    candidates: impl IntoIterator<Item = (&'a str, &'a str)>,
    range: TimeRange,
    filter: &UsageFilter,
    membership_index_file: Option<&str>,
) -> Result<Vec<String>> {
    let candidates = candidates.into_iter().collect::<BTreeMap<_, _>>();
    if candidates.is_empty()
        || filter
            .thread_ids
            .as_ref()
            .is_some_and(std::collections::BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    let mut predicates = Vec::new();
    for (kind, id) in [
        ("account", &filter.account_id),
        ("repository", &filter.repository_id),
        ("model", &filter.model),
    ] {
        if let Some(id) = id {
            predicates.push(format!(
                "(dimension_kind = {} AND dimension_id = {})",
                quoted(kind),
                quoted(id)
            ));
        }
    }
    if let Some(thread) = &filter.thread_id {
        if filter
            .thread_ids
            .as_ref()
            .is_some_and(|ids| !ids.contains(thread))
        {
            return Ok(Vec::new());
        }
        predicates.push(format!(
            "(dimension_kind = 'thread' AND dimension_id = {})",
            quoted(thread)
        ));
    } else if let Some(threads) = &filter.thread_ids {
        let ids = threads
            .iter()
            .map(|id| quoted(id))
            .collect::<Vec<_>>()
            .join(", ");
        predicates.push(format!(
            "(dimension_kind = 'thread' AND dimension_id IN ({ids}))"
        ));
    }
    if predicates.is_empty() {
        return Ok(candidates.into_keys().map(str::to_string).collect());
    }
    let source = if let Some(file) = membership_index_file {
        parquet_source(root, &[file.to_string()])?
    } else {
        parquet_source(
            root,
            &candidates
                .values()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
        )?
    };
    let query = format!(
        "SELECT segment_file FROM {source} WHERE \
         (unknown_start OR min_started_at_ms IS NULL OR min_started_at_ms < {}) AND \
         (unknown_end OR max_ended_at_ms IS NULL OR max_ended_at_ms >= {}) AND \
         ({}) GROUP BY segment_file HAVING count(DISTINCT dimension_kind) = {} \
         ORDER BY segment_file",
        range.end_ms(),
        range.start_ms(),
        predicates.join(" OR "),
        predicates.len()
    );
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let mut statement = connection.prepare(&query).map_err(query_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(query_error)?;
    rows.map(|row| {
        let file = row.map_err(query_error)?;
        if !candidates.contains_key(file.as_str()) {
            return Err(crate::review_state::invalid(
                "membership references an unselected file",
            ));
        }
        Ok(file)
    })
    .collect()
}
