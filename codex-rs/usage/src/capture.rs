use std::path::Path;

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
    UsageJournal::new(codex_home.join(USAGE_DIRECTORY_NAME).join("events.jsonl"))
        .append_record(record)
}
