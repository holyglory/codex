use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::sync::Arc;

use arrow_array::Array;
use arrow_array::ArrayRef;
use arrow_array::Int64Array;
use arrow_array::RecordBatch;
use arrow_array::StringArray;
use arrow_array::UInt64Array;
use arrow_schema::DataType;
use arrow_schema::Field;
use arrow_schema::Schema;
use bytes::Bytes;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use parquet::schema::types::ColumnPath;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::StorageError;
use crate::model::HOUR_SECONDS;
use crate::model::TimeRange;
use crate::model::UsageCorrection;
use crate::model::UsageFilter;
use crate::model::UsageRecord;
use crate::model::UsageTotals;

/// Storage encodings supported by the segment boundary.
///
/// The default build uses [`SegmentFormat::JsonLines`]. Arrow, Parquet, and
/// DuckDB adapters can implement [`SegmentCodec`] without changing query or
/// journal code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentFormat {
    /// Portable JSON object per segment, terminated by a newline.
    JsonLines,
    /// Reserved for an Arrow IPC adapter.
    Arrow,
    /// Reserved for a Parquet adapter.
    Parquet,
    /// Reserved for a DuckDB adapter.
    DuckDb,
}

/// Codec boundary for immutable segments.
pub trait SegmentCodec {
    /// Report the physical format handled by this codec.
    fn format(&self) -> SegmentFormat;

    /// Encode one immutable segment.
    fn encode(&self, segment: &UsageSegment) -> Result<Vec<u8>>;

    /// Decode one immutable segment.
    fn decode(&self, bytes: &[u8]) -> Result<UsageSegment>;
}

/// JSONL codec used when optional columnar dependencies are unavailable.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonLinesSegmentCodec;

impl SegmentCodec for JsonLinesSegmentCodec {
    fn format(&self) -> SegmentFormat {
        SegmentFormat::JsonLines
    }

    fn encode(&self, segment: &UsageSegment) -> Result<Vec<u8>> {
        let envelope = SegmentEnvelope {
            hour_start: segment.hour_start,
            records: segment.records().to_vec(),
        };
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn decode(&self, bytes: &[u8]) -> Result<UsageSegment> {
        let mut reader = BufReader::new(bytes);
        let mut line = String::new();
        let mut envelope = None;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            if envelope.is_some() {
                return Err(StorageError::InvalidData(
                    "one segment JSONL payload must contain one envelope".into(),
                ));
            }
            envelope = Some(serde_json::from_str::<SegmentEnvelope>(line.trim_end())?);
        }
        let envelope = envelope
            .ok_or_else(|| StorageError::InvalidData("segment JSONL payload is empty".into()))?;
        UsageSegment::new(envelope.hour_start, envelope.records)
    }
}

/// Marker for a future Arrow IPC adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArrowSegmentCodec;

impl SegmentCodec for ArrowSegmentCodec {
    fn format(&self) -> SegmentFormat {
        SegmentFormat::Arrow
    }

    fn encode(&self, _segment: &UsageSegment) -> Result<Vec<u8>> {
        Err(StorageError::UnsupportedFormat("Arrow"))
    }

    fn decode(&self, _bytes: &[u8]) -> Result<UsageSegment> {
        Err(StorageError::UnsupportedFormat("Arrow"))
    }
}

/// Marker for a future Parquet adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParquetSegmentCodec;

impl SegmentCodec for ParquetSegmentCodec {
    fn format(&self) -> SegmentFormat {
        SegmentFormat::Parquet
    }

    fn encode(&self, segment: &UsageSegment) -> Result<Vec<u8>> {
        if segment.records().is_empty() {
            return Err(StorageError::InvalidData(
                "an empty segment cannot be encoded as Parquet".into(),
            ));
        }
        let schema = parquet_schema();
        let hour_starts = Int64Array::from(vec![segment.hour_start(); segment.records().len()]);
        let ids = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| Some(record.id.as_str()))
                .collect::<Vec<_>>(),
        );
        let observed_at = Int64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.observed_at)
                .collect::<Vec<_>>(),
        );
        let operation_ids = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.operation_id.as_deref())
                .collect::<Vec<_>>(),
        );
        let thread_ids = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.thread_id.as_deref())
                .collect::<Vec<_>>(),
        );
        let repository_ids = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.repository_id.as_deref())
                .collect::<Vec<_>>(),
        );
        let accounts = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.account_id.as_deref())
                .collect::<Vec<_>>(),
        );
        let tools = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.tool.as_deref())
                .collect::<Vec<_>>(),
        );
        let activities = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.activity.as_deref())
                .collect::<Vec<_>>(),
        );
        let phases = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.phase.as_deref())
                .collect::<Vec<_>>(),
        );
        let classifications = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.classification.as_deref())
                .collect::<Vec<_>>(),
        );
        let events = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.event.as_deref())
                .collect::<Vec<_>>(),
        );
        let coverages = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.coverage.as_deref())
                .collect::<Vec<_>>(),
        );
        let started_at = Int64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.started_at)
                .collect::<Vec<_>>(),
        );
        let ended_at = Int64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.ended_at)
                .collect::<Vec<_>>(),
        );
        let models = StringArray::from(
            segment
                .records()
                .iter()
                .map(|record| record.model.as_deref())
                .collect::<Vec<_>>(),
        );
        let input_tokens = UInt64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.input_tokens)
                .collect::<Vec<_>>(),
        );
        let cached_input_tokens = UInt64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.cached_input_tokens)
                .collect::<Vec<_>>(),
        );
        let output_tokens = UInt64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.output_tokens)
                .collect::<Vec<_>>(),
        );
        let reasoning_output_tokens = UInt64Array::from(
            segment
                .records()
                .iter()
                .map(|record| record.reasoning_output_tokens)
                .collect::<Vec<_>>(),
        );
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(hour_starts) as ArrayRef,
                Arc::new(ids),
                Arc::new(observed_at),
                Arc::new(operation_ids),
                Arc::new(thread_ids),
                Arc::new(repository_ids),
                Arc::new(accounts),
                Arc::new(models),
                Arc::new(tools),
                Arc::new(activities),
                Arc::new(phases),
                Arc::new(classifications),
                Arc::new(events),
                Arc::new(coverages),
                Arc::new(started_at),
                Arc::new(ended_at),
                Arc::new(input_tokens),
                Arc::new(cached_input_tokens),
                Arc::new(output_tokens),
                Arc::new(reasoning_output_tokens),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.parent_thread_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.turn_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.response_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.outcome.map(crate::UsageOutcome::as_str))
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.duration_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.attempt_number)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.provider_total_tokens)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.cache_write_input_tokens)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.missing_observations)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.rework_of_operation_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.observed_at_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.started_at_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.ended_at_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.process_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.agent_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| {
                            record
                                .approval_decision
                                .map(crate::UsageApprovalDecision::as_str)
                        })
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| {
                            record
                                .approval_source
                                .map(crate::UsageApprovalSource::as_str)
                        })
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.taxonomy_version)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(UInt64Array::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.heartbeat_count)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    segment
                        .records()
                        .iter()
                        .map(|record| record.retry_of_operation_id.as_deref())
                        .collect::<Vec<_>>(),
                )),
            ],
        )
        .map_err(columnar_error)?;
        let mut bytes = Vec::new();
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(Default::default()))
            .set_max_row_group_row_count(Some(4_096))
            .set_column_dictionary_enabled(ColumnPath::from("id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("operation_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("thread_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("repository_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("account_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("model"), true)
            .set_column_dictionary_enabled(ColumnPath::from("tool"), true)
            .set_column_dictionary_enabled(ColumnPath::from("activity"), true)
            .set_column_dictionary_enabled(ColumnPath::from("phase"), true)
            .set_column_dictionary_enabled(ColumnPath::from("classification"), true)
            .set_column_dictionary_enabled(ColumnPath::from("event"), true)
            .set_column_dictionary_enabled(ColumnPath::from("coverage"), true)
            .set_column_dictionary_enabled(ColumnPath::from("process_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("agent_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("approval_decision"), true)
            .set_column_dictionary_enabled(ColumnPath::from("approval_source"), true)
            .set_column_dictionary_enabled(ColumnPath::from("retry_of_operation_id"), true)
            .build();
        let mut writer =
            ArrowWriter::try_new(&mut bytes, schema, Some(properties)).map_err(columnar_error)?;
        writer.write(&batch).map_err(columnar_error)?;
        writer.close().map_err(columnar_error)?;
        Ok(bytes)
    }

    fn decode(&self, bytes: &[u8]) -> Result<UsageSegment> {
        let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
            .map_err(columnar_error)?
            .build()
            .map_err(columnar_error)?;
        let mut records = Vec::new();
        let mut hour_start = None;
        for batch in reader {
            let batch = batch.map_err(columnar_error)?;
            let hours = downcast::<Int64Array>(&batch, 0, "hour_start")?;
            let ids = downcast::<StringArray>(&batch, 1, "id")?;
            let observed_at = downcast::<Int64Array>(&batch, 2, "observed_at")?;
            let operation_ids = downcast::<StringArray>(&batch, 3, "operation_id")?;
            let thread_ids = downcast::<StringArray>(&batch, 4, "thread_id")?;
            let repository_ids = downcast::<StringArray>(&batch, 5, "repository_id")?;
            let accounts = downcast::<StringArray>(&batch, 6, "account_id")?;
            let models = downcast::<StringArray>(&batch, 7, "model")?;
            let tools = downcast::<StringArray>(&batch, 8, "tool")?;
            let activities = downcast::<StringArray>(&batch, 9, "activity")?;
            let phases = downcast::<StringArray>(&batch, 10, "phase")?;
            let classifications = downcast::<StringArray>(&batch, 11, "classification")?;
            let events = downcast::<StringArray>(&batch, 12, "event")?;
            let coverages = downcast::<StringArray>(&batch, 13, "coverage")?;
            let started_at = downcast::<Int64Array>(&batch, 14, "started_at")?;
            let ended_at = downcast::<Int64Array>(&batch, 15, "ended_at")?;
            let input_tokens = downcast::<UInt64Array>(&batch, 16, "input_tokens")?;
            let cached_input_tokens = downcast::<UInt64Array>(&batch, 17, "cached_input_tokens")?;
            let output_tokens = downcast::<UInt64Array>(&batch, 18, "output_tokens")?;
            let reasoning_output_tokens =
                downcast::<UInt64Array>(&batch, 19, "reasoning_output_tokens")?;
            let parent_thread_id = downcast::<StringArray>(&batch, 20, "parent_thread_id")?;
            let turn_id = downcast::<StringArray>(&batch, 21, "turn_id")?;
            let response_id = downcast::<StringArray>(&batch, 22, "response_id")?;
            let outcome = downcast::<StringArray>(&batch, 23, "outcome")?;
            let duration_ms = downcast::<UInt64Array>(&batch, 24, "duration_ms")?;
            let attempt_number = downcast::<UInt64Array>(&batch, 25, "attempt_number")?;
            let provider_total_tokens =
                downcast::<UInt64Array>(&batch, 26, "provider_total_tokens")?;
            let cache_write_input_tokens =
                downcast::<UInt64Array>(&batch, 27, "cache_write_input_tokens")?;
            let missing_observations = downcast::<UInt64Array>(&batch, 28, "missing_observations")?;
            let rework_of_operation_id =
                optional_column::<StringArray>(&batch, "rework_of_operation_id")?;
            let observed_at_ms = optional_column::<Int64Array>(&batch, "observed_at_ms")?;
            let started_at_ms = optional_column::<Int64Array>(&batch, "started_at_ms")?;
            let ended_at_ms = optional_column::<Int64Array>(&batch, "ended_at_ms")?;
            let process_id = optional_column::<StringArray>(&batch, "process_id")?;
            let agent_id = optional_column::<StringArray>(&batch, "agent_id")?;
            let approval_decision = optional_column::<StringArray>(&batch, "approval_decision")?;
            let approval_source = optional_column::<StringArray>(&batch, "approval_source")?;
            let taxonomy_version = optional_column::<UInt64Array>(&batch, "taxonomy_version")?;
            let heartbeat_count = optional_column::<UInt64Array>(&batch, "heartbeat_count")?;
            let retry_of_operation_id =
                optional_column::<StringArray>(&batch, "retry_of_operation_id")?;
            for index in 0..batch.num_rows() {
                let current_hour = hours.value(index);
                if hour_start
                    .replace(current_hour)
                    .is_some_and(|previous| previous != current_hour)
                {
                    return Err(StorageError::InvalidData(
                        "Parquet segment contains multiple hours".into(),
                    ));
                }
                records.push(UsageRecord {
                    id: ids.value(index).to_string(),
                    observed_at: observed_at.value(index),
                    operation_id: (!operation_ids.is_null(index))
                        .then(|| operation_ids.value(index).to_string()),
                    thread_id: (!thread_ids.is_null(index))
                        .then(|| thread_ids.value(index).to_string()),
                    repository_id: (!repository_ids.is_null(index))
                        .then(|| repository_ids.value(index).to_string()),
                    account_id: (!accounts.is_null(index))
                        .then(|| accounts.value(index).to_string()),
                    model: (!models.is_null(index)).then(|| models.value(index).to_string()),
                    tool: (!tools.is_null(index)).then(|| tools.value(index).to_string()),
                    activity: (!activities.is_null(index))
                        .then(|| activities.value(index).to_string()),
                    phase: (!phases.is_null(index)).then(|| phases.value(index).to_string()),
                    classification: (!classifications.is_null(index))
                        .then(|| classifications.value(index).to_string()),
                    event: (!events.is_null(index)).then(|| events.value(index).to_string()),
                    coverage: (!coverages.is_null(index))
                        .then(|| coverages.value(index).to_string()),
                    started_at: (!started_at.is_null(index)).then(|| started_at.value(index)),
                    ended_at: (!ended_at.is_null(index)).then(|| ended_at.value(index)),
                    input_tokens: input_tokens.value(index),
                    cached_input_tokens: cached_input_tokens.value(index),
                    output_tokens: output_tokens.value(index),
                    reasoning_output_tokens: reasoning_output_tokens.value(index),
                    parent_thread_id: (!parent_thread_id.is_null(index))
                        .then(|| parent_thread_id.value(index).to_string()),
                    turn_id: (!turn_id.is_null(index)).then(|| turn_id.value(index).to_string()),
                    response_id: (!response_id.is_null(index))
                        .then(|| response_id.value(index).to_string()),
                    outcome: (!outcome.is_null(index))
                        .then(|| outcome.value(index).parse())
                        .transpose()?,
                    duration_ms: (!duration_ms.is_null(index)).then(|| duration_ms.value(index)),
                    attempt_number: (!attempt_number.is_null(index))
                        .then(|| attempt_number.value(index)),
                    provider_total_tokens: (!provider_total_tokens.is_null(index))
                        .then(|| provider_total_tokens.value(index)),
                    cache_write_input_tokens: (!cache_write_input_tokens.is_null(index))
                        .then(|| cache_write_input_tokens.value(index)),
                    missing_observations: (!missing_observations.is_null(index))
                        .then(|| missing_observations.value(index)),
                    rework_of_operation_id: rework_of_operation_id.and_then(|column| {
                        (!column.is_null(index)).then(|| column.value(index).to_string())
                    }),
                    observed_at_ms: observed_at_ms
                        .and_then(|column| (!column.is_null(index)).then(|| column.value(index))),
                    started_at_ms: started_at_ms
                        .and_then(|column| (!column.is_null(index)).then(|| column.value(index))),
                    ended_at_ms: ended_at_ms
                        .and_then(|column| (!column.is_null(index)).then(|| column.value(index))),
                    process_id: process_id.and_then(|column| {
                        (!column.is_null(index)).then(|| column.value(index).to_string())
                    }),
                    agent_id: agent_id.and_then(|column| {
                        (!column.is_null(index)).then(|| column.value(index).to_string())
                    }),
                    approval_decision: approval_decision
                        .and_then(|column| {
                            (!column.is_null(index)).then(|| column.value(index).parse())
                        })
                        .transpose()?,
                    approval_source: approval_source
                        .and_then(|column| {
                            (!column.is_null(index)).then(|| column.value(index).parse())
                        })
                        .transpose()?,
                    taxonomy_version: taxonomy_version
                        .and_then(|column| (!column.is_null(index)).then(|| column.value(index))),
                    heartbeat_count: heartbeat_count
                        .and_then(|column| (!column.is_null(index)).then(|| column.value(index))),
                    retry_of_operation_id: retry_of_operation_id.and_then(|column| {
                        (!column.is_null(index)).then(|| column.value(index).to_string())
                    }),
                });
            }
        }
        let hour_start = hour_start
            .ok_or_else(|| StorageError::InvalidData("Parquet segment contains no rows".into()))?;
        UsageSegment::new(hour_start, records)
    }
}

/// Immutable correction observations for one UTC hour.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCorrectionSegment {
    hour_start: i64,
    corrections: Arc<[UsageCorrection]>,
}

impl UsageCorrectionSegment {
    /// Build a correction segment and validate its hour boundary.
    pub fn new(segment_hour: i64, mut corrections: Vec<UsageCorrection>) -> Result<Self> {
        if segment_hour.rem_euclid(HOUR_SECONDS) != 0 {
            return Err(StorageError::InvalidData(format!(
                "correction segment hour {segment_hour} is not aligned to an hour"
            )));
        }
        if corrections
            .iter()
            .any(|correction| hour_start(correction.observed_at) != segment_hour)
        {
            return Err(StorageError::InvalidData(
                "correction segment contains an observation from another hour".into(),
            ));
        }
        corrections.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(Self {
            hour_start: segment_hour,
            corrections: Arc::from(corrections.into_boxed_slice()),
        })
    }

    /// Return the aligned UTC-hour start timestamp.
    pub const fn hour_start(&self) -> i64 {
        self.hour_start
    }

    /// Borrow corrections in timestamp/id order.
    pub fn corrections(&self) -> &[UsageCorrection] {
        &self.corrections
    }
}

/// Parquet codec for immutable late-fact correction segments.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParquetCorrectionSegmentCodec;

impl SegmentCodec for ParquetCorrectionSegmentCodec {
    fn format(&self) -> SegmentFormat {
        SegmentFormat::Parquet
    }

    fn encode(&self, _segment: &UsageSegment) -> Result<Vec<u8>> {
        Err(StorageError::UnsupportedFormat(
            "usage correction segments require ParquetCorrectionSegmentCodec",
        ))
    }

    fn decode(&self, _bytes: &[u8]) -> Result<UsageSegment> {
        Err(StorageError::UnsupportedFormat(
            "usage correction segments require ParquetCorrectionSegmentCodec",
        ))
    }
}

impl ParquetCorrectionSegmentCodec {
    /// Encode one correction segment as a compressed, dictionary-encoded Parquet file.
    pub fn encode_corrections(&self, segment: &UsageCorrectionSegment) -> Result<Vec<u8>> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("hour_start", DataType::Int64, false),
            Field::new("id", DataType::Utf8, false),
            Field::new("record_id", DataType::Utf8, false),
            Field::new("observed_at", DataType::Int64, false),
            Field::new("input_tokens_delta", DataType::Int64, false),
            Field::new("cached_input_tokens_delta", DataType::Int64, false),
            Field::new("output_tokens_delta", DataType::Int64, false),
            Field::new("reasoning_output_tokens_delta", DataType::Int64, false),
            Field::new("reason", DataType::Utf8, false),
        ]));
        let rows = segment.corrections();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![segment.hour_start(); rows.len()])) as ArrayRef,
                Arc::new(StringArray::from(
                    rows.iter()
                        .map(|row| Some(row.id.as_str()))
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    rows.iter()
                        .map(|row| Some(row.record_id.as_str()))
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    rows.iter().map(|row| row.observed_at).collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    rows.iter()
                        .map(|row| row.input_tokens_delta)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    rows.iter()
                        .map(|row| row.cached_input_tokens_delta)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    rows.iter()
                        .map(|row| row.output_tokens_delta)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    rows.iter()
                        .map(|row| row.reasoning_output_tokens_delta)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    rows.iter()
                        .map(|row| Some(row.reason.as_str()))
                        .collect::<Vec<_>>(),
                )),
            ],
        )
        .map_err(columnar_error)?;
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(Default::default()))
            .set_column_dictionary_enabled(ColumnPath::from("id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("record_id"), true)
            .set_column_dictionary_enabled(ColumnPath::from("reason"), true)
            .build();
        let mut bytes = Vec::new();
        let mut writer =
            ArrowWriter::try_new(&mut bytes, schema, Some(properties)).map_err(columnar_error)?;
        writer.write(&batch).map_err(columnar_error)?;
        writer.close().map_err(columnar_error)?;
        Ok(bytes)
    }

    /// Decode one correction segment from Parquet bytes.
    pub fn decode_corrections(&self, bytes: &[u8]) -> Result<UsageCorrectionSegment> {
        let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
            .map_err(columnar_error)?
            .build()
            .map_err(columnar_error)?;
        let mut rows = Vec::new();
        let mut hour_start = None;
        for batch in reader {
            let batch = batch.map_err(columnar_error)?;
            let hours = downcast::<Int64Array>(&batch, 0, "hour_start")?;
            let ids = downcast::<StringArray>(&batch, 1, "id")?;
            let record_ids = downcast::<StringArray>(&batch, 2, "record_id")?;
            let observed_at = downcast::<Int64Array>(&batch, 3, "observed_at")?;
            let input = downcast::<Int64Array>(&batch, 4, "input_tokens_delta")?;
            let cached = downcast::<Int64Array>(&batch, 5, "cached_input_tokens_delta")?;
            let output = downcast::<Int64Array>(&batch, 6, "output_tokens_delta")?;
            let reasoning = downcast::<Int64Array>(&batch, 7, "reasoning_output_tokens_delta")?;
            let reasons = downcast::<StringArray>(&batch, 8, "reason")?;
            for index in 0..batch.num_rows() {
                let current_hour = hours.value(index);
                if hour_start
                    .replace(current_hour)
                    .is_some_and(|previous| previous != current_hour)
                {
                    return Err(StorageError::InvalidData(
                        "Parquet correction segment contains multiple hours".into(),
                    ));
                }
                rows.push(UsageCorrection {
                    id: ids.value(index).to_string(),
                    record_id: record_ids.value(index).to_string(),
                    observed_at: observed_at.value(index),
                    input_tokens_delta: input.value(index),
                    cached_input_tokens_delta: cached.value(index),
                    output_tokens_delta: output.value(index),
                    reasoning_output_tokens_delta: reasoning.value(index),
                    reason: reasons.value(index).to_string(),
                });
            }
        }
        let hour_start = hour_start.ok_or_else(|| {
            StorageError::InvalidData("Parquet correction segment contains no rows".into())
        })?;
        UsageCorrectionSegment::new(hour_start, rows)
    }
}

fn parquet_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("hour_start", DataType::Int64, false),
        Field::new("id", DataType::Utf8, false),
        Field::new("observed_at", DataType::Int64, false),
        Field::new("operation_id", DataType::Utf8, true),
        Field::new("thread_id", DataType::Utf8, true),
        Field::new("repository_id", DataType::Utf8, true),
        Field::new("account_id", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("tool", DataType::Utf8, true),
        Field::new("activity", DataType::Utf8, true),
        Field::new("phase", DataType::Utf8, true),
        Field::new("classification", DataType::Utf8, true),
        Field::new("event", DataType::Utf8, true),
        Field::new("coverage", DataType::Utf8, true),
        Field::new("started_at", DataType::Int64, true),
        Field::new("ended_at", DataType::Int64, true),
        Field::new("input_tokens", DataType::UInt64, false),
        Field::new("cached_input_tokens", DataType::UInt64, false),
        Field::new("output_tokens", DataType::UInt64, false),
        Field::new("reasoning_output_tokens", DataType::UInt64, false),
        Field::new("parent_thread_id", DataType::Utf8, true),
        Field::new("turn_id", DataType::Utf8, true),
        Field::new("response_id", DataType::Utf8, true),
        Field::new("outcome", DataType::Utf8, true),
        Field::new("duration_ms", DataType::UInt64, true),
        Field::new("attempt_number", DataType::UInt64, true),
        Field::new(
            "provider_total_tokens",
            DataType::UInt64,
            /*nullable*/ true,
        ),
        Field::new(
            "cache_write_input_tokens",
            DataType::UInt64,
            /*nullable*/ true,
        ),
        Field::new(
            "missing_observations",
            DataType::UInt64,
            /*nullable*/ true,
        ),
        Field::new(
            "rework_of_operation_id",
            DataType::Utf8,
            /*nullable*/ true,
        ),
        Field::new("observed_at_ms", DataType::Int64, /*nullable*/ true),
        Field::new("started_at_ms", DataType::Int64, /*nullable*/ true),
        Field::new("ended_at_ms", DataType::Int64, /*nullable*/ true),
        Field::new("process_id", DataType::Utf8, /*nullable*/ true),
        Field::new("agent_id", DataType::Utf8, /*nullable*/ true),
        Field::new("approval_decision", DataType::Utf8, /*nullable*/ true),
        Field::new("approval_source", DataType::Utf8, /*nullable*/ true),
        Field::new("taxonomy_version", DataType::UInt64, /*nullable*/ true),
        Field::new("heartbeat_count", DataType::UInt64, /*nullable*/ true),
        Field::new(
            "retry_of_operation_id",
            DataType::Utf8,
            /*nullable*/ true,
        ),
    ]))
}

fn downcast<'a, T: arrow_array::Array + 'static>(
    batch: &'a RecordBatch,
    index: usize,
    name: &str,
) -> Result<&'a T> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| {
            StorageError::InvalidData(format!("Parquet column {name} has the wrong type"))
        })
}

fn optional_column<'a, T: arrow_array::Array + 'static>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<Option<&'a T>> {
    batch
        .column_by_name(name)
        .map(|column| {
            column.as_any().downcast_ref::<T>().ok_or_else(|| {
                StorageError::InvalidData(format!("Parquet column {name} has the wrong type"))
            })
        })
        .transpose()
}

fn columnar_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::Columnar(error.to_string())
}

/// Marker for a future DuckDB adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct DuckDbSegmentCodec;

impl SegmentCodec for DuckDbSegmentCodec {
    fn format(&self) -> SegmentFormat {
        SegmentFormat::DuckDb
    }

    fn encode(&self, _segment: &UsageSegment) -> Result<Vec<u8>> {
        Err(StorageError::UnsupportedFormat("DuckDB"))
    }

    fn decode(&self, _bytes: &[u8]) -> Result<UsageSegment> {
        Err(StorageError::UnsupportedFormat("DuckDB"))
    }
}

#[derive(Deserialize, Serialize)]
struct SegmentEnvelope {
    hour_start: i64,
    records: Vec<UsageRecord>,
}

/// Immutable observations for one UTC hour.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSegment {
    hour_start: i64,
    records: Arc<[UsageRecord]>,
}

impl UsageSegment {
    /// Build a segment and validate that every record belongs to its hour.
    pub fn new(segment_hour: i64, mut records: Vec<UsageRecord>) -> Result<Self> {
        if segment_hour.rem_euclid(HOUR_SECONDS) != 0 {
            return Err(StorageError::InvalidData(format!(
                "segment hour {segment_hour} is not aligned to an hour"
            )));
        }
        if records.iter().any(|record| {
            hour_start(record.effective_observed_at_ms().div_euclid(1_000)) != segment_hour
        }) {
            return Err(StorageError::InvalidData(
                "segment contains an observation from another hour".into(),
            ));
        }
        records.sort_by(|left, right| {
            left.effective_observed_at_ms()
                .cmp(&right.effective_observed_at_ms())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(Self {
            hour_start: segment_hour,
            records: Arc::from(records.into_boxed_slice()),
        })
    }

    /// Construct an empty segment for a known hour.
    pub fn empty(hour_start: i64) -> Result<Self> {
        Self::new(hour_start, Vec::new())
    }

    /// Construct a segment from records by deriving the hour from the first one.
    pub fn from_records(records: Vec<UsageRecord>) -> Result<Self> {
        let segment_hour = records.first().map_or(0, |record| {
            hour_start(record.effective_observed_at_ms().div_euclid(1_000))
        });
        Self::new(segment_hour, records)
    }

    /// Return the aligned UTC-hour start timestamp.
    pub const fn hour_start(&self) -> i64 {
        self.hour_start
    }

    /// Borrow the immutable observations in timestamp/id order.
    pub fn records(&self) -> &[UsageRecord] {
        &self.records
    }

    /// Return whether this segment overlaps a timestamp range.
    pub fn overlaps(&self, range: TimeRange) -> bool {
        let (start, end) = range.coarse_seconds();
        start < self.hour_start.saturating_add(HOUR_SECONDS) && end > self.hour_start
    }

    /// Query observations in this segment.
    pub fn query(&self, range: TimeRange, filter: &UsageFilter) -> Vec<UsageRecord> {
        if !self.overlaps(range) {
            return Vec::new();
        }
        self.records
            .iter()
            .filter(|record| {
                range.contains_millis(record.effective_observed_at_ms()) && filter.matches(record)
            })
            .cloned()
            .collect()
    }

    /// Sum observations in this segment for a range and filter.
    pub fn totals(&self, range: TimeRange, filter: &UsageFilter) -> UsageTotals {
        self.query(range, filter)
            .into_iter()
            .fold(UsageTotals::default(), |mut totals, record| {
                totals.add_assign(&record.totals());
                totals
            })
    }
}

/// Mutable builder that emits one validated immutable hourly segment.
#[derive(Debug)]
pub struct UsageSegmentBuilder {
    hour_start: i64,
    records: Vec<UsageRecord>,
}

impl UsageSegmentBuilder {
    /// Start a builder for an aligned hour.
    pub fn new(hour_start: i64) -> Result<Self> {
        if hour_start.rem_euclid(HOUR_SECONDS) != 0 {
            return Err(StorageError::InvalidData(format!(
                "segment hour {hour_start} is not aligned to an hour"
            )));
        }
        Ok(Self {
            hour_start,
            records: Vec::new(),
        })
    }

    /// Add an observation to the builder.
    pub fn push(&mut self, record: UsageRecord) -> Result<()> {
        if hour_start(record.effective_observed_at_ms().div_euclid(1_000)) != self.hour_start {
            return Err(StorageError::InvalidData(
                "segment builder received an observation from another hour".into(),
            ));
        }
        self.records.push(record);
        Ok(())
    }

    /// Freeze the builder into an immutable segment.
    pub fn finish(self) -> Result<UsageSegment> {
        UsageSegment::new(self.hour_start, self.records)
    }
}

/// Return the UTC hour containing a Unix timestamp.
pub const fn hour_start(timestamp: i64) -> i64 {
    timestamp.div_euclid(HOUR_SECONDS) * HOUR_SECONDS
}

/// Read one JSONL segment envelope from a reader.
pub fn read_jsonl_segment(reader: impl Read) -> Result<UsageSegment> {
    let mut bytes = Vec::new();
    BufReader::new(reader).read_to_end(&mut bytes)?;
    JsonLinesSegmentCodec.decode(&bytes)
}
