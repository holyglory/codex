//! Page captured outcome rows without changing their report or inventing bindings.
use super::*;

const PREFIX: &str = "usage-outcomes-v1:";
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

pub(super) fn cursor(value: &str) -> Result<(String, usize), FunctionCallError> {
    let invalid = || tool_error("performance review outcome cursor is invalid");
    if value.len() > 96 {
        return Err(invalid());
    }
    let (snapshot, offset) = value
        .strip_prefix(PREFIX)
        .and_then(|value| value.split_once(':'))
        .ok_or_else(invalid)?;
    if snapshot.len() != 32 || !snapshot.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let index = offset.parse::<usize>().map_err(|_| invalid())?;
    if index == 0 || index.to_string() != offset {
        return Err(invalid());
    }
    Ok((snapshot.to_string(), index))
}

pub(super) fn page(
    report: &Value,
    snapshot: &str,
    offset: usize,
    limit: usize,
) -> Result<Value, FunctionCallError> {
    let rows = report
        .pointer("/outcomes/rows")
        .and_then(Value::as_array)
        .ok_or_else(storage_error)?;
    if offset != 0 && offset >= rows.len() {
        return Err(tool_error("performance review outcome cursor is invalid"));
    }
    // An unobserved outcome set stays empty and uses the normal report bound.
    if rows.is_empty() {
        return Ok(report.clone());
    }
    if serde_json::to_writer(SnapshotBound { bytes: 0 }, report).is_err() {
        return Err(tool_error(
            "performance review outcome snapshot exceeds the safe bound; narrow the scope or time range",
        ));
    }
    let mut value = report.clone();
    let mut end = offset.saturating_add(limit).min(rows.len());
    loop {
        value["outcomes"]["rows"] = json!(&rows[offset..end]);
        value["outcomes"]["nextCursor"] = if end < rows.len() {
            json!(format!("{PREFIX}{snapshot}:{end}"))
        } else {
            Value::Null
        };
        match bounded_output(value.clone()) {
            Ok(output) => return Ok(output.0),
            Err(_) if end > offset + 1 => end -= 1,
            Err(error) => return Err(error),
        }
    }
}

// Count serialization bytes without allocating an oversized serialized copy.
struct SnapshotBound {
    bytes: usize,
}

impl std::io::Write for SnapshotBound {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_SNAPSHOT_BYTES.saturating_sub(self.bytes) {
            return Err(std::io::Error::other("outcome snapshot exceeds its bound"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "outcomes_tests.rs"]
mod tests;
