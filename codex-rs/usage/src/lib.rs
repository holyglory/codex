//! Local-first usage storage primitives.
//!
//! Usage observations are appended to a journal, materialized into immutable
//! hourly Parquet segments, and queried by combining a consistent snapshot
//! with an in-memory tail. DuckDB reads the exact immutable segment list for
//! finite-range and multi-dimensional aggregates. The manifest records
//! snapshot generation, journal watermark, freshness, correction segments, and
//! mergeable totals without opening the retired SQLite report database.

mod capture;
mod correction_retention;
mod detail_retention;
mod duckdb_snapshot;
mod entities;
mod error;
mod id_index;
mod id_index_bloom;
mod id_index_compaction;
mod id_index_run;
mod journal;
mod journal_rotation;
mod lineage;
mod manifest;
mod metadata_cache;
mod model;
mod publisher;
mod query;
mod reader;
mod reader_query;
mod reclamation;
mod report;
mod report_facets;
mod report_page;
mod report_query;
mod report_retries;
mod retention;
mod review_aggregate;
mod review_counts;
mod review_index;
mod review_operation_storage;
mod review_projection;
mod review_retention;
mod review_state;
mod review_storage;
mod rollup_compaction;
mod runtime;
mod segment;
mod storage_file;
mod store;
mod summary;
mod summary_cache;
mod token_rollup;

pub use capture::capture_record;
pub use capture::capture_records;
pub use duckdb_snapshot::DuckDbSnapshot;
pub use entities::USAGE_TAXONOMY_VERSION;
pub use entities::UsageApprovalDecision;
pub use entities::UsageApprovalSource;
pub use error::Result;
pub use error::StorageError;
pub use id_index::IdIndexDescriptor;
pub use id_index::IdIndexWrite;
pub use id_index::IdLookup;
pub use id_index::IdRunDescriptor;
pub use id_index::RecordIdEntry;
pub use id_index::id_hash;
pub use id_index::load_id_index;
pub use id_index::lookup_ids;
pub use id_index::missing_id_index;
pub use id_index::publish_id_index;
pub use journal::JournalCheckpoint;
pub use journal::JournalEntry;
pub use journal::JournalPayload;
pub use journal::JournalPosition;
pub use journal::JournalReadBatch;
pub use journal::JournalReadLimits;
pub use journal::JournalReadStatus;
pub use journal::JournalTailRead;
pub use journal::UsageJournal;
pub use lineage::ThreadLineageScope;
pub use lineage::read_descendant_threads;
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
pub use model::UsageOutcome;
pub use model::UsageRecord;
pub use model::UsageTotals;
pub use publisher::UsagePublisher;
pub use query::InMemoryUsageStore;
pub use query::UsageSnapshot;
pub use query::UsageTail;
pub use reader::RecordCursor;
pub use reader::RecordKind;
pub use reader::RecordPage;
pub use reader::RepositoryUsage;
pub use reader::UsageReader;
pub use report_page::OperationPage;
pub use report_page::OperationSelection;
pub use retention::RetentionPolicy;
pub use review_counts::ReviewCountScope;
pub use review_counts::ReviewCounts;
pub use review_counts::read_review_counts;
pub use review_state::ReviewOperation;
pub use review_state::ReviewOperationKind;
pub use runtime::UsagePublisherRuntime;
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

#[cfg(test)]
#[path = "performance_tests.rs"]
mod performance_tests;
