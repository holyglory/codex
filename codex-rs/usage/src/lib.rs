//! Local-first usage storage primitives.
//!
//! Usage observations are appended to a journal, materialized into immutable
//! hourly Parquet segments, and queried by combining a consistent snapshot
//! with an in-memory tail. DuckDB reads the exact immutable segment list for
//! finite-range and multi-dimensional aggregates. The manifest records
//! snapshot generation, journal watermark, freshness, correction segments, and
//! mergeable totals without opening the retired SQLite report database.

mod duckdb_snapshot;
mod error;
mod journal;
mod manifest;
mod model;
mod query;
mod segment;
mod store;

pub use duckdb_snapshot::DuckDbSnapshot;
pub use error::Result;
pub use error::StorageError;
pub use journal::JournalEntry;
pub use journal::JournalPayload;
pub use journal::JournalPosition;
pub use journal::UsageJournal;
pub use manifest::MANIFEST_FORMAT_VERSION;
pub use manifest::ManifestStore;
pub use manifest::UsageManifest;
pub use model::Freshness;
pub use model::HOUR_SECONDS;
pub use model::JournalSequence;
pub use model::SnapshotGeneration;
pub use model::TimeRange;
pub use model::UsageCorrection;
pub use model::UsageDimensions;
pub use model::UsageFilter;
pub use model::UsageRecord;
pub use model::UsageTotals;
pub use query::InMemoryUsageStore;
pub use query::UsageSnapshot;
pub use query::UsageTail;
pub use segment::ArrowSegmentCodec;
pub use segment::DuckDbSegmentCodec;
pub use segment::JsonLinesSegmentCodec;
pub use segment::ParquetCorrectionSegmentCodec;
pub use segment::ParquetSegmentCodec;
pub use segment::SegmentCodec;
pub use segment::SegmentFormat;
pub use segment::UsageCorrectionSegment;
pub use segment::UsageSegment;
pub use segment::UsageSegmentBuilder;
pub use segment::hour_start;
pub use segment::read_jsonl_segment;
pub use store::USAGE_DIRECTORY_NAME;
pub use store::UsageStore;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
