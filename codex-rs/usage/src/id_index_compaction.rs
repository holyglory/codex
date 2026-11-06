//! Lock-free streaming compaction for immutable record-id runs.
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use crate::JournalCheckpoint;
use crate::Result;
use crate::StorageError;
use crate::id_index::IdIndexDescriptor;
use crate::id_index::IdIndexWrite;
use crate::id_index::IdRunDescriptor;
use crate::id_index::MAX_ID_RUNS;
use crate::id_index::RecordIdEntry;
use crate::id_index::data_path;
use crate::id_index::validate_descriptor;
use crate::id_index_run::RunReader;
use crate::id_index_run::RunWriter;
use crate::storage_file;

/// Start compaction before the bounded foreground run cap is reached.
pub const ID_COMPACTION_TRIGGER: usize = 128;
/// At most this many runs are merged by one bounded job.
pub const MAX_COMPACTION_INPUTS: usize = 8;

/// A stable set of input refs selected from one descriptor generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdCompactionPlan {
    pub source_watermark: JournalCheckpoint,
    pub inputs: Vec<IdRunDescriptor>,
    pub output_level: u8,
}

/// Select one same-level group without opening run data. If no level has eight
/// files, the oldest bounded group is selected once the trigger is exceeded.
pub fn plan_id_compaction(descriptor: &IdIndexDescriptor) -> Option<IdCompactionPlan> {
    if descriptor.runs.len() <= ID_COMPACTION_TRIGGER {
        return None;
    }
    let mut levels = descriptor
        .runs
        .iter()
        .map(|run| run.level)
        .collect::<BTreeSet<_>>();
    while let Some(level) = levels.pop_first() {
        let inputs = descriptor
            .runs
            .iter()
            .filter(|run| run.level == level)
            .take(MAX_COMPACTION_INPUTS)
            .cloned()
            .collect::<Vec<_>>();
        if inputs.len() == MAX_COMPACTION_INPUTS {
            return Some(IdCompactionPlan {
                source_watermark: descriptor.watermark,
                inputs,
                output_level: level.saturating_add(1),
            });
        }
    }
    let mut inputs = descriptor.runs.clone();
    inputs.sort_by_key(|run| (run.level, run.min_sequence, run.data_file.clone()));
    inputs.truncate(MAX_COMPACTION_INPUTS);
    Some(IdCompactionPlan {
        source_watermark: descriptor.watermark,
        output_level: inputs
            .iter()
            .map(|run| run.level)
            .max()
            .unwrap_or_default()
            .saturating_add(1),
        inputs,
    })
}

/// Progress returned after a bounded number of output entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdCompactionProgress {
    /// The job yielded before all inputs were consumed.
    More { emitted_entries: u64 },
    /// The immutable output is ready for a short descriptor-install step.
    Finished(IdRunDescriptor),
}

/// A merge job owns only one decoded head per input plus its output writer.
/// It never acquires the publisher snapshot lock.
pub struct IdCompactionJob {
    _pin: crate::reclamation::SnapshotPin,
    _root: std::path::PathBuf,
    _plan: IdCompactionPlan,
    readers: Vec<RunReader>,
    heads: Vec<Option<RecordIdEntry>>,
    writer: Option<RunWriter>,
    emitted_entries: u64,
    finished: bool,
}

impl IdCompactionJob {
    /// Open input readers and create private orphan-safe output files. No
    /// metadata pointer changes until [`install_id_compaction`] is called.
    pub fn start(root: &Path, plan: IdCompactionPlan, stamp: &str) -> Result<Self> {
        let pin = crate::reclamation::SnapshotPin::acquire(root)?;
        if plan.inputs.is_empty() || plan.inputs.len() > MAX_COMPACTION_INPUTS {
            return Err(invalid("invalid record-id compaction input count"));
        }
        let readers = plan
            .inputs
            .iter()
            .map(|run| RunReader::open(root, run))
            .collect::<Result<Vec<_>>>()?;
        let data_file = format!("record-id-compact-{stamp}-l{}.bin", plan.output_level);
        let fence_file = format!("record-id-compact-fence-{stamp}-l{}.bin", plan.output_level);
        let estimated_entries = plan
            .inputs
            .iter()
            .fold(0_u64, |total, run| total.saturating_add(run.entry_count));
        let writer = RunWriter::new(
            root,
            &data_file,
            &fence_file,
            plan.output_level,
            estimated_entries,
        )?;
        Ok(Self {
            _pin: pin,
            _root: root.to_path_buf(),
            heads: vec![None; readers.len()],
            readers,
            writer: Some(writer),
            _plan: plan,
            emitted_entries: 0,
            finished: false,
        })
    }

    /// Emit no more than `max_entries` canonical ids. Streaming merge keeps
    /// memory bounded independently of historical run size.
    pub fn step(&mut self, max_entries: usize) -> Result<IdCompactionProgress> {
        if max_entries == 0 {
            return Err(invalid("compaction step requires a positive entry limit"));
        }
        if self.finished {
            return Err(invalid("record-id compaction already finished"));
        }
        for index in 0..self.readers.len() {
            if self.heads[index].is_none() {
                self.heads[index] = self.readers[index].next()?;
            }
        }
        for _ in 0..max_entries {
            let Some(index) = self.smallest_head() else {
                let Some(writer) = self.writer.take() else {
                    return Err(invalid("unfinished compaction has no writer"));
                };
                let output = writer.finish()?;
                self.finished = true;
                return Ok(IdCompactionProgress::Finished(output));
            };
            let Some(head) = self.heads[index].as_ref() else {
                return Err(invalid("compaction head disappeared"));
            };
            let id = head.id.clone();
            let Some(mut winner) = self.heads[index].take() else {
                return Err(invalid("compaction head disappeared"));
            };
            self.advance(index)?;
            for other in 0..self.heads.len() {
                if self.heads[other]
                    .as_ref()
                    .is_some_and(|entry| entry.id == id)
                {
                    let Some(candidate) = self.heads[other].take() else {
                        return Err(invalid("matching compaction head disappeared"));
                    };
                    if candidate.first_sequence < winner.first_sequence {
                        winner = candidate;
                    } else if candidate.first_sequence == winner.first_sequence
                        && candidate != winner
                    {
                        return Err(invalid("conflicting record-id facts during compaction"));
                    }
                    self.advance(other)?;
                }
            }
            let Some(writer) = self.writer.as_mut() else {
                return Err(invalid("unfinished compaction has no writer"));
            };
            writer.push(&winner)?;
            self.emitted_entries += 1;
        }
        Ok(IdCompactionProgress::More {
            emitted_entries: self.emitted_entries,
        })
    }

    fn smallest_head(&self) -> Option<usize> {
        self.heads
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry)))
            .min_by(|(_, left), (_, right)| {
                crate::id_index::id_hash(&left.id)
                    .cmp(&crate::id_index::id_hash(&right.id))
                    .then_with(|| left.id.cmp(&right.id))
                    .then_with(|| left.first_sequence.cmp(&right.first_sequence))
            })
            .map(|(index, _)| index)
    }

    fn advance(&mut self, index: usize) -> Result<()> {
        self.heads[index] = self.readers[index].next()?;
        Ok(())
    }
}

/// Install a completed output only if every selected input still belongs to
/// the current descriptor. Streaming work can therefore race publication safely.
pub fn install_id_compaction(
    root: &Path,
    current: &IdIndexDescriptor,
    plan: &IdCompactionPlan,
    output: IdRunDescriptor,
    stamp: &str,
) -> Result<IdIndexWrite> {
    validate_descriptor(root, current)?;
    if current.watermark != plan.source_watermark {
        return Err(invalid("record-id compaction source watermark is stale"));
    }
    let selected = plan
        .inputs
        .iter()
        .map(|run| run.data_file.as_str())
        .collect::<BTreeSet<_>>();
    if !selected
        .iter()
        .all(|file| current.runs.iter().any(|run| run.data_file == *file))
    {
        return Err(invalid("record-id compaction input is no longer pinned"));
    }
    let mut descriptor = current.clone();
    descriptor
        .runs
        .retain(|run| !selected.contains(run.data_file.as_str()));
    descriptor.runs.push(output.clone());
    descriptor
        .runs
        .sort_by_key(|run| (run.level, run.min_sequence, run.data_file.clone()));
    if descriptor.runs.len() > MAX_ID_RUNS {
        return Err(invalid("record-id compaction did not reduce run fanout"));
    }
    let file = format!("record-id-index-{stamp}.json");
    let path = data_path(root, &file)?;
    let mut encoded = serde_json::to_vec_pretty(&descriptor)?;
    encoded.push(b'\n');
    let mut output_file = storage_file::create_new(&path)?;
    output_file.write_all(&encoded)?;
    output_file.sync_all()?;
    storage_file::finish_generated(&path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(IdIndexWrite {
        file,
        descriptor,
        run: Some(output),
    })
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}
