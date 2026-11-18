use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::Result;
use crate::manifest::UsageManifest;
use crate::model::TimeRange;
use crate::model::UsageCorrection;
use crate::model::UsageFilter;
use crate::model::UsageRecord;
use crate::model::UsageTotals;
use crate::segment::UsageSegment;

/// Immutable materialized snapshot over hourly segments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSnapshot {
    manifest: UsageManifest,
    segments: Arc<[UsageSegment]>,
}

impl UsageSnapshot {
    /// Build a snapshot, ordering segments by hour and rejecting duplicate hours.
    pub fn new(manifest: UsageManifest, mut segments: Vec<UsageSegment>) -> Result<Self> {
        segments.sort_by_key(UsageSegment::hour_start);
        if segments
            .windows(2)
            .any(|window| window[0].hour_start() == window[1].hour_start())
        {
            return Err(crate::error::StorageError::InvalidData(
                "snapshot contains duplicate hourly segments".into(),
            ));
        }
        Ok(Self {
            manifest,
            segments: Arc::from(segments.into_boxed_slice()),
        })
    }

    /// Build an empty snapshot with default manifest metadata.
    pub fn empty() -> Self {
        Self {
            manifest: UsageManifest::default(),
            segments: Arc::from(Vec::<UsageSegment>::new().into_boxed_slice()),
        }
    }

    /// Borrow manifest metadata for this snapshot.
    pub fn manifest(&self) -> &UsageManifest {
        &self.manifest
    }

    /// Borrow immutable hourly segments.
    pub fn segments(&self) -> &[UsageSegment] {
        &self.segments
    }

    /// Query observations in the snapshot.
    pub fn query(&self, range: TimeRange, filter: &UsageFilter) -> Vec<UsageRecord> {
        self.segments
            .iter()
            .filter(|segment| segment.overlaps(range))
            .flat_map(|segment| segment.query(range, filter))
            .collect()
    }

    /// Sum observations in the snapshot.
    pub fn totals(&self, range: TimeRange, filter: &UsageFilter) -> UsageTotals {
        self.segments
            .iter()
            .filter(|segment| segment.overlaps(range))
            .fold(UsageTotals::default(), |mut totals, segment| {
                totals.add_assign(&segment.totals(range, filter));
                totals
            })
    }
}

/// Mutable in-memory tail containing observations newer than a snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageTail {
    records: Vec<UsageRecord>,
}

impl UsageTail {
    /// Create an empty tail.
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Append one observation to the tail.
    pub fn append(&mut self, record: UsageRecord) {
        self.records.push(record);
    }

    /// Borrow observations in append order.
    pub fn records(&self) -> &[UsageRecord] {
        &self.records
    }

    /// Return the number of observations in the tail.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Return whether the tail contains no observations.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Query observations in the tail.
    pub fn query(&self, range: TimeRange, filter: &UsageFilter) -> Vec<UsageRecord> {
        self.records
            .iter()
            .filter(|record| {
                range.contains_millis(record.effective_observed_at_ms()) && filter.matches(record)
            })
            .cloned()
            .collect()
    }

    /// Remove all records and return the previous tail.
    pub fn take(&mut self) -> Self {
        Self {
            records: std::mem::take(&mut self.records),
        }
    }
}

/// In-memory query view that combines an immutable snapshot, a hot tail, and
/// corrections awaiting the next snapshot generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InMemoryUsageStore {
    snapshot: UsageSnapshot,
    tail: UsageTail,
    corrections: Vec<UsageCorrection>,
}

impl Default for InMemoryUsageStore {
    fn default() -> Self {
        Self::new(UsageSnapshot::empty())
    }
}

impl InMemoryUsageStore {
    /// Create a query store from a snapshot.
    pub fn new(snapshot: UsageSnapshot) -> Self {
        Self {
            snapshot,
            tail: UsageTail::new(),
            corrections: Vec::new(),
        }
    }

    /// Borrow the immutable snapshot.
    pub fn snapshot(&self) -> &UsageSnapshot {
        &self.snapshot
    }

    /// Borrow the mutable tail.
    pub fn tail(&self) -> &UsageTail {
        &self.tail
    }

    /// Borrow corrections that have not yet been folded into a snapshot.
    pub fn pending_corrections(&self) -> &[UsageCorrection] {
        &self.corrections
    }

    /// Append an observation to the in-memory tail.
    pub fn append(&mut self, record: UsageRecord) {
        self.tail.append(record);
    }

    /// Append a correction without mutating an existing observation.
    pub fn append_correction(&mut self, correction: UsageCorrection) {
        if !self
            .corrections
            .iter()
            .any(|current| current.id == correction.id)
        {
            self.corrections.push(correction);
        }
    }

    /// Install a newer immutable snapshot. The tail remains available and is
    /// deduplicated by record id during queries.
    pub fn install_snapshot(&mut self, snapshot: UsageSnapshot) {
        self.snapshot = snapshot;
    }

    /// Query the union of snapshot and tail records, deduplicated by stable id.
    pub fn query(&self, range: TimeRange, filter: &UsageFilter) -> Vec<UsageRecord> {
        let mut by_id = BTreeMap::new();
        for record in self
            .snapshot
            .query(range, filter)
            .into_iter()
            .chain(self.tail.query(range, filter))
        {
            by_id.entry(record.id.clone()).or_insert(record);
        }
        let mut records: Vec<_> = by_id.into_values().collect();
        records.sort_by(|left, right| {
            left.effective_observed_at_ms()
                .cmp(&right.effective_observed_at_ms())
                .then_with(|| left.id.cmp(&right.id))
        });
        records
    }

    /// Sum the union of snapshot and tail records, applying corrections that
    /// target observations included in the query.
    pub fn totals(&self, range: TimeRange, filter: &UsageFilter) -> UsageTotals {
        let records = self.query(range, filter);
        let ids: std::collections::BTreeSet<_> =
            records.iter().map(|record| record.id.as_str()).collect();
        let mut totals = records
            .iter()
            .fold(UsageTotals::default(), |mut totals, record| {
                totals.add_assign(&record.totals());
                totals
            });
        for correction in &self.corrections {
            if ids.contains(correction.record_id.as_str()) {
                totals.apply_correction(correction);
            }
        }
        totals
    }

    /// Take the current tail for segment materialization.
    pub fn take_tail(&mut self) -> UsageTail {
        self.tail.take()
    }
}
