use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::JournalPayload;
use crate::JournalReadLimits;
use crate::UsageContextSize;
use crate::UsageCostProvenance;
use crate::UsageFilter;
use crate::UsageJournal;
use crate::UsageOutcome;
use crate::UsagePublisher;
use crate::UsageReader;
use crate::UsageRecord;
use crate::review_index::select_review_contribution_files;

fn publish(journal: &UsageJournal, publisher: &mut UsagePublisher, index: u64) -> Result<()> {
    let start = 1_000 + index as i64 * 10;
    let mut fact = UsageRecord::new_millis(
        format!("fact-{index}"),
        start + 100,
        /*input_tokens*/ 3,
        /*output_tokens*/ 2,
    )
    .with_operation(format!("operation-{index}"))
    .with_thread(format!("thread-{}", index % 2))
    .with_repository(if index.is_multiple_of(3) {
        "repo-a"
    } else {
        "repo-b"
    });
    fact.agent_id = fact.thread_id.clone();
    fact.event = Some("model_request_finished".into());
    fact.started_at_ms = Some(start);
    fact.ended_at_ms = Some(start + 100);
    fact.duration_ms = Some(100);
    fact.outcome = Some(UsageOutcome::Completed);
    fact.provider_total_tokens = Some(5);
    fact.coverage = Some("recorded".into());
    if !index.is_multiple_of(4) {
        fact.context_size = Some(UsageContextSize {
            prompt_tokens: Some(3),
            context_tokens: Some(3),
            cache_tokens: Some(0),
            compaction_count: Some(0),
            context_window_tokens: Some(16),
            window_class: Some("short".into()),
        });
        fact.cost_provenance = Some(UsageCostProvenance {
            rate_card: Some("fixture-rate".into()),
            rate_card_version: Some("1".into()),
            pricing_basis: Some("api_equivalent".into()),
            estimated_api_equivalent_cost_micros: Some(0),
            ..Default::default()
        });
        fact.actual_billing_micros = Some(0);
    }
    journal.append_batch(vec![JournalPayload::Record(fact)])?;
    publisher.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 1,
    })?;
    Ok(())
}

#[test]
fn compacted_runs_preserve_public_reports_pinned_prefixes_and_retired_source_selection()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("usage-v2");
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut publisher = UsagePublisher::open_at(&root)?;
    for index in 0..16 {
        publish(&journal, &mut publisher, index)?;
    }
    let pinned = UsageReader::open_at(&root)?;
    let descriptor = load_review_projection_shared(&root, pinned.manifest())?.unwrap();
    let canonical_manifest = std::fs::read(root.join("manifest.json"))?;
    let filter = UsageFilter {
        repository_id: Some("repo-a".into()),
        ..Default::default()
    };
    let baseline = pinned.performance_review(TimeRange::all(), &filter)?;
    assert_eq!(
        select_review_contribution_files(
            &root,
            &descriptor,
            TimeRange::all(),
            &UsageFilter::default()
        )?
        .len(),
        16
    );
    assert!(compact_one(&root)?);
    let new_outputs = retained_files(&root)?;
    assert!(!new_outputs.is_empty());
    for file in &new_outputs {
        // The serving previous collector does not read the optional index; it
        // would remove every unreferenced output in one of these namespaces.
        if [
            "segment-",
            "correction-",
            "membership-",
            "lineage-",
            "record-id-",
            "review-",
            "rollup-",
        ]
        .iter()
        .any(|prefix| file.starts_with(prefix))
        {
            std::fs::remove_file(segment_path(&root, file)?)?;
        }
        assert!(segment_path(&root, file)?.is_file());
    }
    assert_eq!(
        std::fs::read(root.join("manifest.json"))?,
        canonical_manifest
    );
    assert_eq!(
        select_review_contribution_files(
            &root,
            &descriptor,
            TimeRange::all(),
            &UsageFilter::default()
        )?
        .len(),
        1
    );
    assert_eq!(
        UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?,
        baseline
    );
    assert_eq!(
        pinned.performance_review(TimeRange::all(), &filter)?,
        baseline
    );
    // An appended fragment is read beside the compacted prefix immediately.
    publish(&journal, &mut publisher, /*index*/ 16)?;
    let current = ManifestStore::new(root.join("manifest.json")).load()?;
    let appended = load_review_projection_shared(&root, &current)?.unwrap();
    assert_eq!(
        selection(&root, &appended, TimeRange::all()).unwrap().len(),
        2
    );
    assert_eq!(
        pinned.performance_review(TimeRange::all(), &filter)?,
        baseline
    );
    for index in 17..32 {
        publish(&journal, &mut publisher, index)?;
    }
    let current = ManifestStore::new(root.join("manifest.json")).load()?;
    let descriptor = load_review_projection_shared(&root, &current)?.unwrap();
    let before = UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?;
    assert!(compact_one(&root)?);
    let index = load(&root)?.unwrap();
    assert_eq!(index.runs.len(), 2);
    let old_outputs = retained_files(&root)?;
    // Retiring one canonical source invalidates only the run containing it.
    let retired = index.runs[0].sources[0].clone();
    let mut reduced = (*descriptor).clone();
    reduced
        .contribution_fragments
        .retain(|fragment| fragment.file != retired);
    let selected = selection(&root, &reduced, TimeRange::all()).unwrap();
    assert_eq!(selected.len(), 16);
    assert!(
        !selected
            .iter()
            .any(|(file, _)| file == &index.runs[0].data_file)
    );
    assert!(
        selected
            .iter()
            .any(|(file, _)| file == &index.runs[1].data_file)
    );
    // A balanced merge may cover a newer prefix than the old pinned reader.
    assert!(compact_one(&root)?);
    assert!(
        selection(
            &root,
            &load_review_projection_shared(&root, pinned.manifest())?.unwrap(),
            TimeRange::all()
        )
        .is_none()
    );
    assert_eq!(
        pinned.performance_review(TimeRange::all(), &filter)?,
        baseline
    );
    assert_eq!(crate::reclamation::reclaim(&root)?.files_removed, 0);
    for file in &old_outputs {
        assert!(segment_path(&root, file)?.is_file());
    }
    let restarted = UsageReader::open_at(&root)?;
    assert_eq!(
        restarted.performance_review(TimeRange::all(), &filter)?,
        before
    );
    drop(restarted);
    drop(pinned);
    let orphan = root
        .join("segments")
        .join("analytics-review-orphan.parquet");
    std::fs::write(&orphan, b"unpublished derived candidate")?;
    crate::reclamation::reclaim(&root)?;
    assert!(!orphan.exists());
    for file in old_outputs {
        assert!(!segment_path(&root, &file)?.exists());
    }
    for file in retained_files(&root)? {
        assert!(segment_path(&root, &file)?.is_file());
    }
    // Unusable optional metadata and changed output files fall back to originals.
    let saved_index = std::fs::read(root.join(INDEX))?;
    let mut retired: serde_json::Value = serde_json::from_slice(&saved_index)?;
    retired["version"] = json!(1);
    std::fs::write(root.join(INDEX), serde_json::to_vec(&retired)?)?;
    assert!(load(&root).is_err());
    assert!(!has_index(&root));
    assert!(selection(&root, &descriptor, TimeRange::all()).is_none());
    assert_eq!(
        UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?,
        before
    );
    assert!(compact_one(&root)?);
    assert_eq!(
        load(&root)?.expect("normal current-format rebuild").version,
        INDEX_VERSION
    );
    assert_eq!(
        UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?,
        before
    );
    std::fs::write(root.join(INDEX), b"invalid derived index")?;
    assert!(selection(&root, &descriptor, TimeRange::all()).is_none());
    assert_eq!(
        UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?,
        before
    );
    std::fs::write(root.join(INDEX), saved_index)?;
    let compacted = load(&root)?.unwrap();
    let data = segment_path(&root, &compacted.runs[0].data_file)?;
    std::fs::write(&data, vec![0; compacted.runs[0].data_bytes as usize])?;
    assert!(selection(&root, &descriptor, TimeRange::all()).is_none());
    assert_eq!(
        UsageReader::open_at(&root)?.performance_review(TimeRange::all(), &filter)?,
        before
    );
    Ok(())
}

#[test]
fn planning_bounds_file_fanout_rows_bytes_and_coverage_without_dropping_eligible_inputs() {
    // Isolate admission arithmetic; the public-reader lifecycle above exercises
    // real capture/publication and all resulting report fields.
    let input = |ordinal| Run {
        sources: vec![format!("source-{ordinal}")],
        data_file: format!("data-{ordinal}"),
        membership_file: format!("membership-{ordinal}"),
        min_observed_at_ms: 0,
        max_observed_at_ms: 1,
        rows: 1,
        data_bytes: 1,
        membership_bytes: 1,
        data_modified_ns: Some(1),
        membership_modified_ns: Some(1),
        level: 0,
    };
    let fanout = bounded((0..MAX_INPUT_FILES + 3).map(input));
    assert_eq!(fanout.len(), MAX_INPUT_FILES);
    let mut full_rows = input(0);
    full_rows.rows = MAX_ROWS;
    let mut full_bytes = input(0);
    full_bytes.data_bytes = MAX_BYTES - full_bytes.membership_bytes;
    let mut full_coverage = input(0);
    full_coverage.sources = (0..MAX_SOURCES)
        .map(|index| format!("covered-{index}"))
        .collect();
    for full in [full_rows, full_bytes, full_coverage] {
        assert_eq!(
            bounded([full, input(1)])
                .iter()
                .map(|run| run.data_file.as_str())
                .collect::<Vec<_>>(),
            vec!["data-0"]
        );
    }
    let mut oversized = input(0);
    oversized.rows = MAX_ROWS + 1;
    assert_eq!(
        bounded([oversized, input(1)])
            .iter()
            .map(|run| run.data_file.as_str())
            .collect::<Vec<_>>(),
        vec!["data-1"]
    );
    assert_eq!(
        json!({"rows":fanout.iter().map(|run| run.rows).sum::<u64>(),
        "sources":fanout.iter().map(|run| run.sources.len()).sum::<usize>()}),
        json!({"rows":MAX_INPUT_FILES,"sources":MAX_INPUT_FILES})
    );
}

#[test]
fn oversized_prefix_admission_advances_after_restart_with_bounded_footer_reads() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("usage-v2");
    storage_file::ensure_usage_directory(&root)?;
    let fact = UsageRecord::new_millis(
        "small", /*observed_at_ms*/ 1_000, /*input_tokens*/ 3, /*output_tokens*/ 2,
    );
    let contribution =
        crate::review_state::ReviewContribution::from_fact(/*sequence*/ 1, &fact);
    crate::review_storage::write_rows(
        &root,
        "small.parquet",
        &[contribution],
        crate::review_storage::CONTRIBUTION_COLUMNS,
    )?;
    crate::review_index::write_membership(
        &root,
        "input-membership.parquet",
        "small.parquet",
        [crate::review_index::IndexedInterval {
            account_id: None,
            thread_id: None,
            repository_id: None,
            model: None,
            start: Some(1_000),
            end: Some(1_000),
        }],
    )?;
    let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
    let small_source = parquet_source(&root, &["small.parquet".into()])?;
    let small = ReviewContributionFragment {
        file: "small.parquet".into(),
        membership_file: "input-membership.parquet".into(),
        min_observed_at_ms: 1_000,
        max_observed_at_ms: 1_000,
    };
    for row_count in [MAX_ROWS, MAX_ROWS + 1] {
        let large_file = format!("large-{row_count}.parquet");
        let large_path = segment_path(&root, &large_file)?;
        connection.execute_batch(&format!(
            "SET threads=1; SET memory_limit='128MB'; \
             COPY (SELECT s.* REPLACE ('large-' || r.i::VARCHAR AS record_id, (r.i+1)::UBIGINT AS source_sequence) \
             FROM {small_source} s CROSS JOIN range({row_count}) r(i)) TO {} (FORMAT PARQUET, COMPRESSION ZSTD)",
            quoted(&large_path.to_string_lossy()))).map_err(query_error)?;
        storage_file::finish_generated(&large_path)?;
        let mut large = small.clone();
        large.file = large_file;
        // Logical descriptors reuse these real small/large Parquet footers.
        // The lifecycle test separately proves actual canonical publication.
        let fragments = (0..MAX_INPUT_FILES + 2)
            .map(|ordinal| {
                let mut fragment = small.clone();
                fragment.file = if ordinal < MAX_INPUT_FILES {
                    format!("large-{ordinal}")
                } else {
                    format!("ordinary-{ordinal}")
                };
                fragment
            })
            .collect::<Vec<_>>();
        let reads = std::cell::Cell::new(0_usize);
        let read = |logical: &ReviewContributionFragment| -> Result<FragmentInput> {
            reads.set(reads.get() + 1);
            let physical = if logical.file.starts_with("large-") {
                &large
            } else {
                &small
            };
            match fragment_input(&root, physical)? {
                FragmentInput::Eligible(mut input) => {
                    input.sources = vec![logical.file.clone()];
                    input.data_file = logical.file.clone();
                    Ok(FragmentInput::Eligible(input))
                }
                FragmentInput::Unmergeable => Ok(FragmentInput::Unmergeable),
            }
        };
        let mut index = Index {
            version: INDEX_VERSION,
            runs: Vec::new(),
            unmergeable_sources: BTreeSet::new(),
        };
        let (first, advanced) = scan_fragments(&fragments, &mut index.unmergeable_sources, read)?;
        assert!(advanced);
        assert_eq!(first.len(), usize::from(row_count <= MAX_ROWS));
        assert_eq!(reads.get(), MAX_INPUT_FILES);
        assert_eq!(index.unmergeable_sources.len(), MAX_INPUT_FILES);
        let _pin = SnapshotPin::acquire(&root)?;
        let publisher = storage_file::open_read_write(&root.join(".snapshot.lock"))?;
        publisher.lock()?;
        save(&root, &index, &stamp()?)?;
        drop(publisher);
        let mut restarted = (*load(&root)?.unwrap()).clone();
        assert!(
            !has_index(&root),
            "admission hints never substitute canonical sources"
        );
        reads.set(0);
        let (later, advanced) =
            scan_fragments(&fragments, &mut restarted.unmergeable_sources, read)?;
        assert!(!advanced);
        assert_eq!(reads.get(), 2);
        assert_eq!(
            later
                .iter()
                .map(|run| run.data_file.as_str())
                .collect::<Vec<_>>(),
            fragments[MAX_INPUT_FILES..]
                .iter()
                .map(|fragment| fragment.file.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(later.iter().map(|run| run.rows).sum::<u64>(), 2);
        if row_count == MAX_ROWS {
            // A single valid full-size file cannot make 127 small peers
            // permanently unmergeable: pack those peers together first.
            let mut mixed = fragments.clone();
            for (ordinal, fragment) in mixed.iter_mut().enumerate().take(MAX_INPUT_FILES).skip(1) {
                fragment.file = format!("ordinary-{ordinal}");
            }
            let mut unmergeable = BTreeSet::new();
            reads.set(0);
            let (packed, advanced) = scan_fragments(&mixed, &mut unmergeable, read)?;
            assert!(!advanced);
            assert!(unmergeable.is_empty());
            assert_eq!(reads.get(), MAX_INPUT_FILES);
            assert_eq!(
                packed
                    .iter()
                    .flat_map(|run| run.sources.iter().cloned())
                    .collect::<BTreeSet<_>>(),
                mixed[1..MAX_INPUT_FILES]
                    .iter()
                    .map(|fragment| fragment.file.clone())
                    .collect::<BTreeSet<_>>()
            );
        }
    }
    Ok(())
}
