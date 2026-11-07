//! Bounded operation-address merging and retirement on the existing worker.
use std::fs::TryLockError;
use std::path::Path;
use std::sync::Arc;

use crate::{ManifestStore, Result};
use crate::detail_retention::stamp;
use crate::id_index::IdRunDescriptor;
use crate::id_index::id_hash;
use crate::id_index_compaction::{ID_COMPACTION_TRIGGER, MAX_COMPACTION_INPUTS};
use crate::id_index_run::{RunReader, RunWriter};
use crate::operation_id_index::{self, OperationIdEntry, OperationIdIndex, OperationIdRun};
use crate::reclamation::SnapshotPin;
use crate::review_projection::{self, ReviewProjectionDescriptor};
use crate::review_state::{REVIEW_PARTITION_SIZE, invalid};
use crate::review_storage::write_json;
use crate::storage_file;

#[derive(Default)]
pub(crate) struct Maintenance { job: Option<Job> }

struct Job {
    _pin: SnapshotPin,
    source: Arc<ReviewProjectionDescriptor>,
    inputs: Vec<OperationIdRun>,
    readers: Vec<RunReader>,
    heads: Vec<Option<OperationIdEntry>>,
    pending: Option<OperationIdEntry>,
    writer: Option<RunWriter>,
    output: Option<IdRunDescriptor>,
}

impl Maintenance {
    /// Consume at most max_entries input addresses, including retired entries.
    /// The pinned job survives publisher appends and lock contention.
    pub(crate) fn step(&mut self, root: &Path, max_entries: usize) -> Result<bool> {
        if max_entries == 0 { return Err(invalid("operation-id step needs a positive budget")); }
        let pin = SnapshotPin::acquire(root)?;
        let store = ManifestStore::new(root.join("manifest.json"));
        let manifest = store.load()?;
        let Some(source) = review_projection::load_review_projection_shared(root, &manifest)? else { return Ok(false); };
        let Some(index) = operation_id_index::load(root, &source)? else { return Ok(false); };
        if self.job.as_ref().is_some_and(|job| !job.inputs.iter().all(|run| index.runs.contains(run))) {
            self.job = None;
        }
        if self.job.is_none() {
            let Some(inputs) = select_inputs(&index, source.retired_operation_rows) else { return Ok(false); };
            self.job = Some(Job::start(root, source, inputs, pin)?);
        }
        let Some(job) = self.job.as_mut() else { return Ok(false); };
        if !job.step(max_entries)? { return Ok(true); }

        let lock = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut current = store.load()?;
        let Some(mut review) = review_projection::load_review_projection(root, &current)? else { self.job = None; return Ok(false); };
        let Some(mut index) = operation_id_index::load(root, &review)? else { self.job = None; return Ok(false); };
        // New publications may append runs while this job is streaming. Only
        // immutable selected inputs are replaced; all new refs stay pinned.
        if !job.inputs.iter().all(|run| index.runs.contains(run)) {
            self.job = None;
            return Ok(true);
        }
        index.runs.retain(|run| !job.inputs.contains(run));
        let output = job.output.as_ref().ok_or_else(|| invalid("operation-id merge has no output"))?;
        if output.entry_count != 0 {
            index.runs.push(OperationIdRun {
                run: output.clone(), retired_through: job.source.retired_operation_rows,
            });
        }
        let stamp = stamp();
        review.operation_id_index_file = Some(operation_id_index::write(root, &index, &stamp)?);
        let file = format!("review-projection-{stamp}.json");
        write_json(root, &file, &review)?;
        current.review_projection_file = Some(file);
        #[cfg(unix)]
        std::fs::File::open(root.join("segments"))?.sync_all()?;
        current.mark_snapshot(current.watermark, current.freshness);
        store.save(&current)?;
        self.job = None;
        Ok(true)
    }
}

fn select_inputs(index: &OperationIdIndex, revision: u64) -> Option<Vec<OperationIdRun>> {
    if index.runs.len() > ID_COMPACTION_TRIGGER {
        for level in index.runs.iter().map(|entry| entry.run.level).collect::<std::collections::BTreeSet<_>>() {
            let inputs = index.runs.iter().filter(|entry| entry.run.level == level)
                .take(MAX_COMPACTION_INPUTS).cloned().collect::<Vec<_>>();
            if inputs.len() == MAX_COMPACTION_INPUTS { return Some(inputs); }
        }
        let mut inputs = index.runs.clone();
        inputs.sort_by_key(|entry| (entry.run.level, entry.run.min_sequence));
        inputs.truncate(MAX_COMPACTION_INPUTS);
        Some(inputs)
    } else {
        index.runs.iter().find(|entry| entry.retired_through < revision)
            .map(|entry| vec![entry.clone()])
    }
}

impl Job {
    fn start(root: &Path, source: Arc<ReviewProjectionDescriptor>, inputs: Vec<OperationIdRun>, pin: SnapshotPin) -> Result<Self> {
        if inputs.is_empty() || inputs.len() > MAX_COMPACTION_INPUTS {
            return Err(invalid("invalid operation-id merge input count"));
        }
        let readers = inputs.iter().map(|entry| RunReader::open(root, &entry.run)).collect::<Result<Vec<_>>>()?;
        let level = inputs.iter().map(|entry| entry.run.level).max().unwrap_or_default()
            .saturating_add(u8::from(inputs.len() > 1));
        let estimated = inputs.iter().fold(0_u64, |total, entry| total.saturating_add(entry.run.entry_count));
        let stamp = stamp();
        let writer = RunWriter::new(root, &format!("review-operation-id-merge-{stamp}.bin"),
            &format!("review-operation-id-merge-fence-{stamp}.bin"), level, estimated)?;
        Ok(Self {
            _pin: pin, source, inputs, heads: vec![None; readers.len()], readers,
            pending: None, writer: Some(writer), output: None,
        })
    }

    fn step(&mut self, max_entries: usize) -> Result<bool> {
        if self.output.is_some() { return Ok(true); }
        for index in 0..self.readers.len() {
            if self.heads[index].is_none() { self.heads[index] = self.readers[index].next_entry()?; }
        }
        for _ in 0..max_entries {
            let next = self.heads.iter().enumerate()
                .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry)))
                .min_by(|(_, a), (_, b)| (id_hash(&a.operation_id), &a.operation_id, a.first_sequence)
                    .cmp(&(id_hash(&b.operation_id), &b.operation_id, b.first_sequence)))
                .map(|(index, _)| index);
            let Some(index) = next else {
                let mut writer = self.writer.take().ok_or_else(|| invalid("operation-id merge has no writer"))?;
                if let Some(entry) = self.pending.take() { writer.push_entry(&entry)?; }
                self.output = Some(writer.finish()?);
                return Ok(true);
            };
            let entry = self.heads[index].take().ok_or_else(|| invalid("operation-id merge lost its head"))?;
            self.heads[index] = self.readers[index].next_entry()?;
            operation_id_index::validate_entry(&entry, self.source.watermark.sequence)?;
            let partition = (entry.first_sequence - 1) / REVIEW_PARTITION_SIZE;
            let active = self.source.partitions.binary_search_by_key(&partition, |p| p.partition)
                .ok().is_some_and(|i| operation_id_index::contains(&self.source.partitions[i], entry.first_sequence));
            // Filter each candidate before choosing a winner: a retired early
            // sequence must not shadow a later reappearance of the same ID.
            if !active { continue; }
            if let Some(previous) = self.pending.as_ref()
                && previous.operation_id == entry.operation_id
            { return Err(invalid("conflicting live operation addresses during merge")); }
            if let Some(previous) = self.pending.replace(entry) {
                self.writer.as_mut().ok_or_else(|| invalid("operation-id merge has no writer"))?.push_entry(&previous)?;
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
#[path = "operation_id_maintenance_tests.rs"]
mod tests;
