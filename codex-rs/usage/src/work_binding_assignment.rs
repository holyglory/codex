//! Local derived dimensions; missing assignment evidence never blocks publication.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use crate::OperationBindingMetadata;
use crate::ReviewOperation;
use crate::UsageReader;
use crate::UsageRecord;
use crate::WorkBindingTransition;
use crate::review_state::lifecycle;
use crate::work_binding_snapshot::WorkBindingSnapshot;

#[derive(Clone)]
struct Assignment {
    project: String,
    stream: Option<String>,
    outcome: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Origin {
    Canonical,
    CurrentBatch,
}

#[derive(Clone)]
struct Candidate {
    first: u64,
    anchor: Option<u64>,
    thread: Option<String>,
    retry: Option<String>,
    assignment: Option<Assignment>,
    origin: Origin,
}

impl Candidate {
    fn from_operation(row: &ReviewOperation) -> Self {
        Self {
            first: row.first_sequence,
            anchor: row.work_binding_start_sequence,
            thread: row.thread_id.clone(),
            retry: row.retry_of_operation_id.clone(),
            assignment: row
                .work_native_project_id
                .as_ref()
                .map(|project| Assignment {
                    project: project.clone(),
                    stream: row.workstream_id.clone(),
                    outcome: row.work_outcome_id.clone(),
                }),
            origin: Origin::Canonical,
        }
    }

    fn from_metadata(row: OperationBindingMetadata) -> Self {
        Self {
            first: row.first_sequence,
            anchor: row.work_binding_start_sequence,
            thread: row.thread_id,
            retry: row.retry_of_operation_id,
            assignment: row.work_native_project_id.map(|project| Assignment {
                project,
                stream: row.workstream_id,
                outcome: row.work_outcome_id,
            }),
            origin: Origin::Canonical,
        }
    }
}

struct Resolver<'a> {
    root: &'a Path,
    candidates: BTreeMap<String, Candidate>,
    checked: BTreeSet<String>,
    resolved: BTreeMap<String, Option<Assignment>>,
    reader: OnceLock<Option<UsageReader>>,
    bindings: BTreeMap<&'a str, Vec<&'a WorkBindingTransition>>,
}

impl Resolver<'_> {
    fn candidate(&mut self, id: &str) -> Option<Candidate> {
        if self
            .candidates
            .get(id)
            .is_some_and(|row| row.origin == Origin::Canonical)
            || self.checked.contains(id)
        {
            return self.candidates.get(id).cloned();
        }
        let reader = self
            .reader
            .get_or_init(|| UsageReader::open_at(self.root).ok())
            .as_ref()?;
        let metadata = reader
            .operation_binding_metadata(&BTreeSet::from([id.to_owned()]))
            .ok()?;
        if let Some(metadata) = metadata.into_iter().next() {
            self.candidates
                .insert(id.to_owned(), Candidate::from_metadata(metadata));
        }
        self.checked.insert(id.to_owned());
        self.candidates.get(id).cloned()
    }

    fn resolve(&mut self, id: &str, depth: usize) -> Option<Assignment> {
        if let Some(result) = self.resolved.get(id) {
            return result.clone();
        }
        if depth == 64 {
            return None;
        }
        let row = self.candidate(id)?;
        if row.anchor != Some(row.first) || row.first == 0 {
            return None;
        }
        let result = if let Some(assignment) = row.assignment {
            Some(assignment)
        } else if let Some(parent) = &row.retry {
            let predecessor = self.candidate(parent)?;
            if predecessor.thread != row.thread || predecessor.first >= row.first {
                return None;
            }
            self.resolve(parent, depth + 1)
        } else {
            let bindings = self.bindings.get(row.thread.as_deref()?)?;
            let index =
                bindings.partition_point(|binding| binding.effective_capture_sequence <= row.first);
            index.checked_sub(1).and_then(|index| {
                let update = &bindings[index].update;
                update.native_project_id.as_ref().map(|project| Assignment {
                    project: project.clone(),
                    stream: update.workstream_id.clone(),
                    outcome: update.outcome_id.clone(),
                })
            })
        };
        self.resolved.insert(id.to_owned(), result.clone());
        result
    }
}

/// Fill derived storage fields only. Unavailable/corrupt sources and ambiguous
/// starts stay null; ordinary token and operation publication must still proceed.
pub(crate) fn assign_work_dimensions(
    root: &Path,
    rows: &mut [ReviewOperation],
    records: &[(u64, UsageRecord)],
) {
    for row in rows.iter_mut() {
        if row.work_binding_start_sequence != Some(row.first_sequence)
            || row.work_native_project_id.is_none()
        {
            row.work_native_project_id = None;
            row.workstream_id = None;
            row.work_outcome_id = None;
        }
    }
    if rows.iter().all(|row| {
        row.work_native_project_id.is_some() || row.work_binding_start_sequence.is_none()
    }) {
        return;
    }
    let snapshot = WorkBindingSnapshot::pin(root);
    let mut bindings = BTreeMap::<&str, Vec<&WorkBindingTransition>>::new();
    for binding in snapshot.transitions() {
        bindings
            .entry(&binding.update.thread_id)
            .or_default()
            .push(binding);
    }
    if bindings.is_empty() && rows.iter().all(|row| row.retry_of_operation_id.is_none()) {
        return;
    }
    let mut candidates = rows
        .iter()
        .map(|row| (row.operation_id.clone(), Candidate::from_operation(row)))
        .collect::<BTreeMap<_, _>>();
    for (sequence, record) in records {
        let Some((_, terminal)) = lifecycle(record) else {
            continue;
        };
        let Some(id) = &record.operation_id else {
            continue;
        };
        let candidate = Candidate {
            first: *sequence,
            anchor: (!terminal).then_some(*sequence),
            thread: record.thread_id.clone(),
            retry: record.retry_of_operation_id.clone(),
            assignment: None,
            origin: Origin::CurrentBatch,
        };
        let entry = candidates
            .entry(id.clone())
            .or_insert_with(|| candidate.clone());
        if entry.origin == Origin::CurrentBatch && *sequence < entry.first {
            *entry = candidate;
        }
    }
    let mut resolver = Resolver {
        root,
        candidates,
        checked: BTreeSet::new(),
        resolved: BTreeMap::new(),
        reader: OnceLock::new(),
        bindings,
    };
    for row in rows {
        if row.work_native_project_id.is_some() {
            continue;
        }
        if let Some(assignment) = resolver.resolve(&row.operation_id, 0) {
            row.work_native_project_id = Some(assignment.project);
            row.workstream_id = assignment.stream;
            row.work_outcome_id = assignment.outcome;
        }
    }
}
