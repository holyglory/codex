//! Typed Arrow construction for the bounded canonical operation partition.
use std::path::Path;
use std::sync::Arc;

use arrow_array::ArrayRef;
use arrow_array::BooleanArray;
use arrow_array::Int64Array;
use arrow_array::RecordBatch;
use arrow_array::StringArray;
use arrow_array::UInt64Array;
use arrow_schema::Field;
use arrow_schema::Schema;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::Result;
use crate::StorageError;
use crate::UsageOutcome;
use crate::review_state::ReviewOperation;
use crate::review_state::ReviewOperationKind;
use crate::review_state::invalid;
use crate::review_storage::OPERATION_COLUMNS;
use crate::review_storage::segment_path;
use crate::storage_file;

pub(crate) fn write_operation_rows(
    root: &Path,
    file: &str,
    rows: &[ReviewOperation],
) -> Result<()> {
    #[cfg(test)]
    let mut profile = crate::performance_tests::RecoveryProfile::new("operation-arrow-write");
    if rows.is_empty() {
        return Err(invalid("empty review Parquet file"));
    }
    // Borrow strings directly from their typed rows. No JSON object or cloned
    // optional field is needed between the operation state and Arrow buffers.
    macro_rules! column {
        ($array:ident, |$row:ident| $value:expr) => {
            Arc::new($array::from_iter(rows.iter().map(|$row| $value))) as ArrayRef
        };
    }
    let arrays = vec![
        column!(StringArray, |row| Some(row.operation_id.as_str())),
        column!(StringArray, |row| Some(match row.kind {
            ReviewOperationKind::Tool => "tool",
            ReviewOperationKind::ModelRequest => "model_request",
            ReviewOperationKind::ModelWarmup => "model_warmup",
            ReviewOperationKind::Wait => "wait",
        })),
        column!(UInt64Array, |row| Some(row.first_sequence)),
        column!(UInt64Array, |row| Some(row.source_sequence)),
        column!(Int64Array, |row| Some(row.observed_at_ms)),
        column!(BooleanArray, |row| Some(row.start_observed)),
        column!(BooleanArray, |row| Some(row.terminal_observed)),
        column!(Int64Array, |row| row.started_at_ms),
        column!(Int64Array, |row| row.ended_at_ms),
        column!(UInt64Array, |row| row.duration_ms),
        column!(StringArray, |row| row.outcome.map(
            |outcome| match outcome {
                UsageOutcome::Completed => "completed",
                UsageOutcome::Failed => "failed",
                UsageOutcome::Blocked => "blocked",
                UsageOutcome::Cancelled => "cancelled",
                UsageOutcome::Incomplete => "incomplete",
            }
        )),
        column!(StringArray, |row| row.account_id.as_deref()),
        column!(StringArray, |row| row.thread_id.as_deref()),
        column!(StringArray, |row| row.repository_id.as_deref()),
        column!(StringArray, |row| row.turn_id.as_deref()),
        column!(StringArray, |row| row.model.as_deref()),
        column!(StringArray, |row| row.tool.as_deref()),
        column!(StringArray, |row| row.response_id.as_deref()),
        column!(StringArray, |row| row.phase.as_deref()),
        column!(StringArray, |row| row.activity.as_deref()),
        column!(StringArray, |row| row.classification.as_deref()),
        column!(StringArray, |row| row.coverage.as_deref()),
        column!(UInt64Array, |row| row.attempt_number),
        column!(StringArray, |row| row.rework_of_operation_id.as_deref()),
        column!(StringArray, |row| row.process_id.as_deref()),
        column!(StringArray, |row| row.agent_id.as_deref()),
        column!(StringArray, |row| row.retry_of_operation_id.as_deref()),
    ];
    let fields = OPERATION_COLUMNS
        .iter()
        .map(|(name, kind)| Field::new(*name, kind.clone(), /*nullable*/ true))
        .collect::<Vec<_>>();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), arrays)
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    #[cfg(test)]
    profile.mark("arrow-construction");
    let path = segment_path(root, file)?;
    let output = storage_file::create_new(&path)?;
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        // Preserve the current repository/time pruning behavior for comparison.
        .set_max_row_group_row_count(Some(256))
        .build();
    let mut writer = ArrowWriter::try_new(output, schema, Some(properties))
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    writer
        .write(&batch)
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    writer
        .close()
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    storage_file::finish_generated(&path)?;
    #[cfg(test)]
    profile.mark("parquet-write");
    Ok(())
}
