//! Typed immutable Parquet files for bounded review partitions and contributions.
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use arrow_array::ArrayRef;
use arrow_array::BooleanArray;
use arrow_array::Int64Array;
use arrow_array::RecordBatch;
use arrow_array::StringArray;
use arrow_array::UInt64Array;
use arrow_schema::DataType;
use arrow_schema::Field;
use arrow_schema::Schema;
use duckdb::Connection;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Result;
use crate::StorageError;
use crate::review_state::invalid;
use crate::storage_file;

macro_rules! columns {
    ($($name:ident:$kind:ident),* $(,)?) => {
        &[$((stringify!($name), DataType::$kind)),*]
    };
}

pub(crate) const OPERATION_COLUMNS: &[(&str, DataType)] = columns![
    operation_id:Utf8, kind:Utf8, first_sequence:UInt64, source_sequence:UInt64,
    observed_at_ms:Int64, start_observed:Boolean, terminal_observed:Boolean,
    started_at_ms:Int64, ended_at_ms:Int64, duration_ms:UInt64, outcome:Utf8,
    account_id:Utf8, thread_id:Utf8, repository_id:Utf8, turn_id:Utf8,
    model:Utf8, tool:Utf8, response_id:Utf8, phase:Utf8, activity:Utf8,
    classification:Utf8, coverage:Utf8, attempt_number:UInt64, rework_of_operation_id:Utf8,
];
pub(crate) const OPERATION_INDEX_COLUMNS: &[(&str, DataType)] = columns![
    operation_id:Utf8, first_sequence:UInt64,
];
pub(crate) const CONTRIBUTION_COLUMNS: &[(&str, DataType)] = columns![
    record_id:Utf8, source_sequence:UInt64, observed_at_ms:Int64,
    operation_id:Utf8, response_id:Utf8, account_id:Utf8, thread_id:Utf8,
    repository_id:Utf8, model:Utf8, tool:Utf8, event:Utf8, coverage:Utf8,
    phase:Utf8, activity:Utf8, classification:Utf8, input_tokens:UInt64,
    cached_input_tokens:UInt64, output_tokens:UInt64, reasoning_output_tokens:UInt64,
    provider_total_tokens:UInt64, cache_write_input_tokens:UInt64,
    missing_observations:UInt64, attempt_number:UInt64, rework_of_operation_id:Utf8,
];
pub(crate) const CLASSIFICATION_COLUMNS: &[(&str, DataType)] = columns![
    record_id:Utf8, operation_id:Utf8, source_sequence:UInt64, observed_at_ms:Int64,
    phase:Utf8, activity:Utf8, classification:Utf8,
];
pub(crate) const MEMBERSHIP_COLUMNS: &[(&str, DataType)] = columns![
    dimension_kind:Utf8, dimension_id:Utf8, segment_file:Utf8,
    min_started_at_ms:Int64, max_ended_at_ms:Int64, unknown_start:Boolean, unknown_end:Boolean,
];

pub(crate) fn write_rows<T: Serialize>(
    root: &Path,
    file: &str,
    rows: &[T],
    columns: &[(&str, DataType)],
) -> Result<()> {
    if rows.is_empty() {
        return Err(invalid("empty review Parquet file"));
    }
    let values = rows
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let fields = columns
        .iter()
        .map(|(name, kind)| Field::new(*name, kind.clone(), /*nullable*/ true))
        .collect::<Vec<_>>();
    let schema = Arc::new(Schema::new(fields));
    let arrays = columns
        .iter()
        .map(|(name, kind)| {
            let cells = values.iter().map(|row| &row[*name]);
            let array: ArrayRef = match kind {
                DataType::Utf8 => Arc::new(StringArray::from(
                    cells.map(serde_json::Value::as_str).collect::<Vec<_>>(),
                )),
                DataType::Int64 => Arc::new(Int64Array::from(
                    cells.map(serde_json::Value::as_i64).collect::<Vec<_>>(),
                )),
                DataType::UInt64 => Arc::new(UInt64Array::from(
                    cells.map(serde_json::Value::as_u64).collect::<Vec<_>>(),
                )),
                DataType::Boolean => Arc::new(BooleanArray::from(
                    cells.map(serde_json::Value::as_bool).collect::<Vec<_>>(),
                )),
                _ => return Err(invalid("unsupported review column type")),
            };
            Ok(array)
        })
        .collect::<Result<Vec<_>>>()?;
    let batch = RecordBatch::try_new(Arc::clone(&schema), arrays)
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    let path = segment_path(root, file)?;
    let output = storage_file::create_new(&path)?;
    let max_row_group_rows = if columns == OPERATION_COLUMNS {
        // Operation rows are sorted by repository before publication. Small
        // groups let DuckDB skip unrelated repositories for finite reviews.
        256
    } else {
        64_000
    };
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(Default::default()))
        // Keep dimension-sorted review files in bounded row groups so DuckDB
        // can use Parquet statistics to skip unrelated dimensions.
        .set_max_row_group_row_count(Some(max_row_group_rows))
        .build();
    let mut writer = ArrowWriter::try_new(output, schema, Some(properties))
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    writer
        .write(&batch)
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    writer
        .close()
        .map_err(|error| StorageError::Columnar(error.to_string()))?;
    storage_file::finish_generated(&path)
}

pub(crate) fn write_json<T: Serialize>(root: &Path, file: &str, value: &T) -> Result<()> {
    let encoded = serde_json::to_vec(value)?;
    let mut output = storage_file::create_new(&segment_path(root, file)?)?;
    output.write_all(&encoded)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    Ok(())
}

/// Deserialize only explicitly selected rows, with a hard caller-provided cap.
pub(crate) fn query_rows<T: DeserializeOwned>(
    connection: &Connection,
    source: &str,
    max_rows: usize,
) -> Result<Vec<T>> {
    let limit = max_rows.saturating_add(1);
    let mut statement = connection
        .prepare(&format!(
            "SELECT to_json(r) FROM ({source}) AS r LIMIT {limit}"
        ))
        .map_err(query_error)?;
    let mut rows = statement.query([]).map_err(query_error)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().map_err(query_error)? {
        if result.len() == max_rows {
            return Err(invalid("review row selection exceeds its bounded capacity"));
        }
        let json: String = row.get(0).map_err(query_error)?;
        result.push(serde_json::from_str(&json)?);
    }
    Ok(result)
}

pub(crate) fn parquet_source(root: &Path, files: &[String]) -> Result<String> {
    if files.is_empty() {
        return Err(invalid("review Parquet source requires at least one file"));
    }
    let paths = files
        .iter()
        .map(|file| {
            let path = segment_path(root, file)?;
            Ok(quoted(&path.to_string_lossy()))
        })
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    Ok(format!("read_parquet([{paths}], union_by_name=true)"))
}

pub(crate) fn segment_path(root: &Path, file: &str) -> Result<PathBuf> {
    if Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file) {
        return Err(invalid("review segment must be a file name"));
    }
    Ok(root.join("segments").join(file))
}

pub(crate) fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn query_error(error: duckdb::Error) -> StorageError {
    StorageError::Columnar(format!("DuckDB review query failed: {error}"))
}
