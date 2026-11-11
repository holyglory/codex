//! Sparse, explicit assignments. The capture journal lock orders switches with facts.
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::StorageError;
use crate::USAGE_DIRECTORY_NAME;
use crate::UsageJournal;
use crate::storage_file;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TRANSITIONS: usize = 65_536;

/// One explicit assignment for a thread. Null project and work IDs explicitly clear it.
/// Producers send this only when the job changes, never with individual operations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkBindingUpdate {
    pub version: u32,
    pub native_project_id: Option<String>,
    pub thread_id: String,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
    /// Monotonically increasing producer sequence within this thread.
    pub transition_sequence: u64,
    /// Opaque producer identifier, such as `project_automation`.
    pub source: String,
    /// Explicit assignment or correction authority; never inferred by the collector.
    pub authority: String,
}

/// Immutable assignment fact. The collector supplies its capture boundary and time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkBindingTransition {
    pub update: WorkBindingUpdate,
    pub effective_capture_sequence: u64,
    pub captured_at_ms: i64,
}

impl WorkBindingTransition {
    /// Stable context identity for report joins; it contains no user content.
    pub fn context_id(&self) -> String {
        format!(
            "binding:{}:{}",
            self.update.thread_id, self.update.transition_sequence
        )
    }
}

/// Append one out-of-band switch, serialized with every normal usage append.
/// A retry of an identical accepted switch is idempotent; stale or conflicting
/// sequences fail without changing the active assignment.
pub fn capture_work_binding_transition(
    codex_home: &Path,
    update: WorkBindingUpdate,
) -> Result<WorkBindingTransition> {
    validate(&update)?;
    let root = codex_home.join(USAGE_DIRECTORY_NAME);
    let guard = storage_file::open_read_write(&root.join("events.lock"))?;
    guard.lock()?;
    let path = root.join("work-bindings.jsonl");
    let (transitions, complete_bytes) = read(&path)?;
    if let Some(previous) = transitions
        .iter()
        .rev()
        .find(|transition| transition.update.thread_id == update.thread_id)
    {
        if previous.update == update {
            return Ok(previous.clone());
        }
        if update.transition_sequence <= previous.update.transition_sequence {
            return Err(invalid("stale or conflicting work binding transition"));
        }
    }
    if transitions.len() == MAX_TRANSITIONS {
        return Err(invalid("work binding transition journal at capacity"));
    }
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let transition = WorkBindingTransition {
        update,
        effective_capture_sequence: journal
            .last_sequence()?
            .checked_add(1)
            .ok_or_else(|| invalid("work binding capture sequence exhausted"))?,
        captured_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| invalid("work binding clock unavailable"))?
            .as_millis()
            .try_into()
            .map_err(|_| invalid("work binding clock overflow"))?,
    };
    let mut bytes = serde_json::to_vec(&transition)?;
    bytes.push(b'\n');
    if complete_bytes.saturating_add(bytes.len() as u64) > MAX_BYTES {
        return Err(invalid("work binding transition journal at capacity"));
    }
    // Discard only an interrupted final append, under the same writer lock.
    let recovery = storage_file::open_read_write(&path)?;
    recovery.set_len(complete_bytes)?;
    drop(recovery);
    let mut file = storage_file::open_append(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(&root)?.sync_all()?;
    Ok(transition)
}

/// Pin the complete transition prefix for one reader. A later switch cannot
/// affect facts already in that reader's captured journal prefix.
pub fn read_work_binding_transitions(codex_home: &Path) -> Result<Vec<WorkBindingTransition>> {
    let root = codex_home.join(USAGE_DIRECTORY_NAME);
    read_transitions_at(&root)
}

pub(crate) fn read_transitions_at(root: &Path) -> Result<Vec<WorkBindingTransition>> {
    let guard = storage_file::open_read_write(&root.join("events.lock"))?;
    guard.lock_shared()?;
    read(&root.join("work-bindings.jsonl")).map(|(transitions, _)| transitions)
}

fn read(path: &Path) -> Result<(Vec<WorkBindingTransition>, u64)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error.into()),
    };
    storage_file::protect_existing(path)?;
    let length = file.metadata()?.len();
    read_prefix(&mut file, length)
}

pub(crate) fn read_prefix(
    file: &mut File,
    length: u64,
) -> Result<(Vec<WorkBindingTransition>, u64)> {
    if length > MAX_BYTES {
        return Err(invalid("work binding transition journal exceeds bounds"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(file).take(length).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return Err(invalid("work binding snapshot prefix changed"));
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("work binding transition journal exceeds bounds"));
    }
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut transitions = Vec::<WorkBindingTransition>::new();
    let mut latest = std::collections::BTreeMap::<String, u64>::new();
    let mut effective = 0;
    for line in bytes[..complete]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if transitions.len() == MAX_TRANSITIONS {
            return Err(invalid("work binding transition journal exceeds bounds"));
        }
        let transition: WorkBindingTransition = serde_json::from_slice(line)?;
        validate(&transition.update)?;
        if transition.effective_capture_sequence == 0
            || transition.effective_capture_sequence < effective
            || transition.captured_at_ms < 0
            || latest
                .get(&transition.update.thread_id)
                .is_some_and(|sequence| transition.update.transition_sequence <= *sequence)
        {
            return Err(invalid("invalid work binding transition order"));
        }
        effective = transition.effective_capture_sequence;
        latest.insert(
            transition.update.thread_id.clone(),
            transition.update.transition_sequence,
        );
        transitions.push(transition);
    }
    Ok((transitions, complete as u64))
}

fn validate(update: &WorkBindingUpdate) -> Result<()> {
    if update.version != 1
        || update.transition_sequence == 0
        || update.native_project_id.is_none()
            && (update.workstream_id.is_some() || update.outcome_id.is_some())
        || [
            Some(update.thread_id.as_str()),
            update.native_project_id.as_deref(),
            update.workstream_id.as_deref(),
            update.outcome_id.as_deref(),
            Some(update.source.as_str()),
            Some(update.authority.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        })
    {
        return Err(invalid("invalid content-free work binding transition"));
    }
    Ok(())
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}
