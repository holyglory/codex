use std::path::Path;

use crate::JournalPayload;
use crate::Result;
use crate::USAGE_DIRECTORY_NAME;
use crate::UsageJournal;
use crate::UsageRecord;
use crate::journal::JournalPosition;

/// Persist an observation without loading any report or immutable snapshot.
///
/// Capture owns only journal append. A broken or slow query engine must not
/// prevent new observations from becoming durable.
pub fn capture_record(codex_home: &Path, record: UsageRecord) -> Result<JournalPosition> {
    let position = UsageJournal::new(codex_home.join(USAGE_DIRECTORY_NAME).join("events.jsonl"))
        .append_record(record)?;
    crate::runtime::notify_publisher(codex_home);
    Ok(position)
}

/// Persist one bounded callback batch with a single durability barrier.
/// The caller keeps ownership of retry policy; publication is only notified
/// after the entire batch is acknowledged by the journal.
pub fn capture_records(
    codex_home: &Path,
    records: Vec<UsageRecord>,
) -> Result<Vec<JournalPosition>> {
    let positions = UsageJournal::new(codex_home.join(USAGE_DIRECTORY_NAME).join("events.jsonl"))
        .append_batch(records.into_iter().map(JournalPayload::Record).collect())?;
    if !positions.is_empty() {
        crate::runtime::notify_publisher(codex_home);
    }
    Ok(positions)
}
