use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::GlobalReviewReader;
use crate::JournalReadLimits;
use crate::UsageContextSize;
use crate::UsageCostProvenance;
use crate::UsageFilter;
use crate::UsageOutcome;
use crate::UsagePublisher;
use crate::UsageRecord;

fn observation(id: &str, at: i64, total: u64) -> UsageRecord {
    let mut record =
        UsageRecord::new_millis(id, at, /*input_tokens*/ 11, /*output_tokens*/ 3)
            .with_operation(id)
            .with_thread("agent")
            .with_repository("repository");
    record.event = Some("model_request_finished".into());
    record.started_at_ms = Some(at - 100);
    record.ended_at_ms = Some(at);
    record.outcome = Some(UsageOutcome::Completed);
    record.coverage = Some("recorded".into());
    record.provider_total_tokens = Some(total);
    record.context_size = Some(UsageContextSize {
        prompt_tokens: Some(11),
        context_tokens: Some(11),
        cache_tokens: Some(2),
        context_window_tokens: Some(128_000),
        compaction_count: Some(0),
        window_class: Some("short".into()),
    });
    record.cost_provenance = Some(UsageCostProvenance {
        model: Some("model".into()),
        provider: Some("provider".into()),
        service_tier: Some("standard".into()),
        context_class: Some("short".into()),
        region: None,
        rate_card: Some("trusted".into()),
        rate_card_version: Some("v1".into()),
        pricing_basis: Some("api_equivalent".into()),
        estimated_api_equivalent_cost_micros: Some(7),
    });
    record
}

fn capture(root: &Path, id: &str, at: i64, total: u64) {
    UsageJournal::new(root.join("events.jsonl"))
        .append_record(observation(id, at, total))
        .expect("durable capture");
}

fn canonical(root: &Path) {
    UsagePublisher::open_at(root)
        .expect("canonical publisher")
        .publish_batch(JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 64,
        })
        .expect("canonical snapshot");
}

fn publish(root: &Path) {
    assert_eq!(
        GlobalReviewPublisher::open_at(root)
            .expect("global builder")
            .expect("changed prefix")
            .publish()
            .expect("complete background publication"),
        GlobalReviewPublication::Published
    );
}

#[test]
fn global_analytics_background_snapshot_matches_native_and_preserves_source_during_capture() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    assert!(
        GlobalReviewReader::open_at(&root)
            .expect("unpublished read")
            .is_none()
    );
    capture(&root, "first", /*at*/ 1_000, /*total*/ 57);
    canonical(&root);
    let expected = UsageReader::open_at(&root)
        .expect("reference prefix")
        .performance_review(TimeRange::all(), &UsageFilter::default())
        .expect("native report");
    publish(&root);
    let old = GlobalReviewReader::open_at(&root)
        .expect("cached report")
        .expect("complete publication");
    assert_eq!(old.report(), &expected);
    let prefix = old.snapshot().included_journal_watermark;
    assert_eq!(
        (
            old.snapshot().source_watermark,
            prefix,
            old.snapshot().pending_entries
        ),
        (1, 1, 0)
    );
    assert!(
        GlobalReviewPublisher::open_at(&root)
            .expect("unchanged prefix")
            .is_none()
    );
    capture(&root, "second", /*at*/ 2_000, /*total*/ 61);
    let waiting = GlobalReviewReader::open_at(&root)
        .expect("backlog metadata")
        .expect("old completed report");
    assert_eq!(waiting.report(), old.report());
    assert_eq!(
        (
            waiting.snapshot().included_journal_watermark,
            waiting.snapshot().latest_journal_watermark,
            waiting.snapshot().pending_entries,
            waiting.snapshot().freshness
        ),
        (prefix, 2, 1, Freshness::Stale)
    );
    std::thread::scope(|scope| {
        let builder = scope.spawn(|| publish(&root));
        for _ in 0..16 {
            assert_eq!(old.report(), &expected);
        }
        builder.join().expect("independent publication");
    });
    let current = GlobalReviewReader::open_at(&root)
        .expect("restart reader")
        .expect("new completed report");
    assert_eq!(
        (
            current.snapshot().source_watermark,
            current.snapshot().included_journal_watermark
        ),
        (1, 2)
    );
    assert_ne!(old.snapshot().snapshot_id, current.snapshot().snapshot_id);
    for snapshot in [old.snapshot(), current.snapshot()] {
        let parts = snapshot.snapshot_id.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts[..3],
            [
                "global".to_owned(),
                format!("g{}", snapshot.source_snapshot_generation),
                format!("j{}", snapshot.included_journal_watermark)
            ]
        );
        assert!(parts[3].parse::<u128>().is_ok());
    }
    assert_eq!(current.report()["coverage"]["rawTokenObservations"], 2);
    assert_eq!(old.report(), &expected);
    // Cached JSON owns its value; historical source files can be reclaimed.
    crate::reclamation::reclaim(&root).expect("no report JSON canonical-file pin");
    assert_eq!(old.report(), &expected);
    codex_private_storage::verify_private_file(&root.join(PUBLICATION))
        .expect("private completed JSON");
    assert!(!std::fs::read_dir(&root).expect("root").any(|entry| {
        entry
            .expect("entry")
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("global-review-work-"))
    }));
    // Foreground lookup remains independent even when all rich metadata is unavailable.
    std::fs::rename(
        root.join("manifest.json"),
        root.join("manifest-hidden.json"),
    )
    .expect("hide large manifest");
    std::fs::rename(root.join("segments"), root.join("segments-hidden"))
        .expect("hide all source Parquet");
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("JSON-only read")
            .expect("published report")
            .report(),
        current.report()
    );
}

#[test]
fn global_analytics_same_prefix_store_replacement_and_metadata_coverage_rebuild() {
    let home = tempdir().expect("home");
    let root = home
        .path()
        .join("original")
        .join(crate::USAGE_DIRECTORY_NAME);
    capture(
        &root,
        "same-operation",
        /*at*/ 1_000,
        /*total*/ 57,
    );
    canonical(&root);
    let manifest_path = root.join("manifest.json");
    // Match the existing same-size/mtime replacement contract, with room for
    // harmless JSON whitespace if immutable file-name lengths happen to differ.
    let mut original = std::fs::read(&manifest_path).expect("original manifest");
    original.resize(original.len() + 4_096, b' ');
    std::fs::write(&manifest_path, &original).expect("padded original fixture");
    publish(&root);
    let old = GlobalReviewReader::open_at(&root)
        .expect("old reader")
        .expect("old completed facts");
    let prefix = (
        old.snapshot().source_snapshot_generation,
        old.snapshot().source_watermark,
        old.snapshot().included_journal_watermark,
    );
    let metadata = std::fs::metadata(&manifest_path).expect("original source identity");
    let other = home
        .path()
        .join("replacement")
        .join(crate::USAGE_DIRECTORY_NAME);
    capture(
        &other,
        "same-operation",
        /*at*/ 1_000,
        /*total*/ 61,
    );
    canonical(&other);
    for entry in std::fs::read_dir(other.join("segments")).expect("replacement immutable graph") {
        let entry = entry.expect("immutable file");
        std::fs::copy(entry.path(), root.join("segments").join(entry.file_name()))
            .expect("copy new canonical source file");
    }
    std::fs::copy(other.join("events.jsonl"), root.join("events.jsonl"))
        .expect("replacement matching journal");
    let mut replacement = std::fs::read(other.join("manifest.json")).expect("replacement manifest");
    assert!(replacement.len() <= original.len());
    replacement.resize(original.len(), b' ');
    let temporary = root.join("manifest.replacement");
    std::fs::write(&temporary, replacement).expect("new source inode");
    std::fs::File::options()
        .write(true)
        .open(&temporary)
        .expect("replacement source")
        .set_times(
            std::fs::FileTimes::new().set_modified(metadata.modified().expect("original mtime")),
        )
        .expect("same mtime");
    std::fs::rename(&temporary, &manifest_path).expect("atomic same-prefix source replacement");
    let replaced = std::fs::metadata(&manifest_path).expect("new manifest identity");
    assert_eq!(
        (replaced.len(), replaced.modified().expect("new mtime")),
        (metadata.len(), metadata.modified().expect("old mtime"))
    );
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("completed old snapshot still readable")
            .expect("completed")
            .report(),
        old.report()
    );
    publish(&root);
    let current = GlobalReviewReader::open_at(&root)
        .expect("replacement publication")
        .expect("rebuilt report");
    assert_eq!(
        (
            current.snapshot().source_snapshot_generation,
            current.snapshot().source_watermark,
            current.snapshot().included_journal_watermark
        ),
        prefix
    );
    assert_eq!(
        current.report()["outcomes"]["totals"]["providerTotalTokens"]["measured"],
        61
    );
    assert_eq!(
        old.report()["outcomes"]["totals"]["providerTotalTokens"]["measured"],
        57
    );
    assert_ne!(current.snapshot().snapshot_id, old.snapshot().snapshot_id);
    assert_eq!(
        current.report(),
        &UsageReader::open_at(&root)
            .expect("replacement canonical reader")
            .performance_review(TimeRange::all(), &UsageFilter::default())
            .expect("replacement native report")
    );
    assert!(
        GlobalReviewPublisher::open_at(&root)
            .expect("unchanged exact source identity")
            .is_none()
    );

    // A metadata-only authoritative save may change coverage without advancing
    // either numeric frontier. The next derived publication must capture it.
    let store = crate::ManifestStore::new(&manifest_path);
    let mut changed = store.load().expect("current manifest");
    changed.unapplied_retention_records = 1;
    changed.unapplied_retention_corrections = 2;
    store
        .save(&changed)
        .expect("same-prefix retention coverage change");
    let changed_bytes = std::fs::read(&manifest_path).expect("authoritative source bytes");
    publish(&root);
    let coverage = GlobalReviewReader::open_at(&root)
        .expect("coverage reader")
        .expect("new metadata publication");
    assert_eq!(
        (
            coverage.snapshot().source_snapshot_generation,
            coverage.snapshot().source_watermark,
            coverage.snapshot().included_journal_watermark
        ),
        prefix
    );
    assert_eq!(
        (
            coverage.snapshot().unapplied_retention_records,
            coverage.snapshot().unapplied_retention_corrections
        ),
        (1, 2)
    );
    assert_eq!(
        coverage.report()["coverage"]["unappliedObservationsAfterRetention"],
        1
    );
    assert_eq!(
        coverage.report()["coverage"]["unappliedCorrectionsAfterRetention"],
        2
    );
    assert_eq!(
        coverage.report()["outcomes"]["totals"]["providerTotalTokens"]["exact"],
        serde_json::Value::Null
    );
    assert_eq!(
        std::fs::read(manifest_path).expect("canonical source preserved by rebuild"),
        changed_bytes
    );
}

#[test]
fn global_analytics_failed_candidate_preserves_previous_report_and_cleans_private_state() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    capture(&root, "first", /*at*/ 1_000, /*total*/ 57);
    canonical(&root);
    publish(&root);
    let before = std::fs::read(root.join(PUBLICATION)).expect("completed bytes");
    capture(&root, "second", /*at*/ 2_000, /*total*/ 61);
    let path = {
        let mut builder = GlobalReviewPublisher::open_at(&root)
            .expect("builder")
            .expect("new prefix");
        assert!(
            GlobalReviewPublisher::open_at(&root)
                .expect("single writer")
                .is_none()
        );
        let path = builder.work.0.clone();
        // A real filesystem failure prevents query setup before any pointer changes.
        std::fs::remove_dir_all(&path).expect("remove candidate directory");
        std::fs::write(&path, b"not a directory").expect("candidate filesystem failure");
        assert!(builder.publish().is_err());
        assert_eq!(
            std::fs::read(root.join(PUBLICATION)).expect("preserved publication"),
            before
        );
        std::fs::remove_file(&path).expect("release injected failure");
        path
    };
    assert!(!path.exists());
    publish(&root);
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("retry")
            .expect("complete")
            .snapshot()
            .included_journal_watermark,
        2
    );
}

#[test]
fn global_analytics_background_resources_are_bounded_and_ordinary_tables_are_persistent() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    capture(&root, "first", /*at*/ 1_000, /*total*/ 57);
    canonical(&root);
    let home = root.parent().expect("fixture home");
    for (thread, outcome) in [("overlap-a", "a"), ("overlap-b", "b")] {
        crate::capture_work_binding_transition(
            home,
            crate::WorkBindingUpdate {
                version: 1,
                native_project_id: Some("project".into()),
                thread_id: thread.into(),
                workstream_id: Some("stream".into()),
                outcome_id: Some(outcome.into()),
                transition_sequence: 1,
                source: "fixture".into(),
                authority: "agent_declared".into(),
            },
        )
        .expect("explicit outcome");
    }
    let mut records = Vec::new();
    for index in 0..2_050 {
        let thread = if index % 2 == 0 {
            "overlap-a"
        } else {
            "overlap-b"
        };
        let mut fact =
            observation(&format!("overlap-{index}"), 2_000, /*total*/ 1).with_thread(thread);
        fact.started_at_ms = Some(1_000 + index % 200);
        fact.agent_id = Some(thread.into());
        let mut started = fact.clone();
        started.id = format!("overlap-{index}:started");
        started.event = Some("model_request_started".into());
        started.ended_at_ms = None;
        started.provider_total_tokens = None;
        started.input_tokens = 0;
        started.output_tokens = 0;
        started.outcome = None;
        started.context_size = None;
        started.cost_provenance = None;
        started.observed_at_ms = started.started_at_ms;
        records.extend([started, fact]);
    }
    let mut missing_end = observation("missing-end", 2_000, /*total*/ 1);
    missing_end.ended_at_ms = None;
    let mut missing_owner = observation("missing-owner", 2_000, /*total*/ 1);
    missing_owner.thread_id = None;
    records.extend([missing_end, missing_owner]);
    let mut zero = observation("zero-duration", 2_000, /*total*/ 1);
    zero.started_at_ms = zero.ended_at_ms;
    records.push(zero);
    for (id, start, end) in [("wait-a", 1_200, 1_800), ("wait-b", 1_500, 1_900)] {
        let mut wait = observation(id, end, /*total*/ 0).with_thread("overlap-a");
        wait.input_tokens = 0;
        wait.output_tokens = 0;
        wait.provider_total_tokens = None;
        wait.started_at_ms = Some(start);
        wait.event = Some("retry_wait_finished".into());
        let mut started = wait.clone();
        started.id = format!("{id}:started");
        started.event = Some("retry_wait_started".into());
        started.ended_at_ms = None;
        started.outcome = None;
        started.observed_at_ms = Some(start);
        started.context_size = None;
        started.cost_provenance = None;
        records.extend([started, wait]);
    }
    UsageJournal::new(root.join("events.jsonl"))
        .append_batch(
            records
                .into_iter()
                .map(crate::JournalPayload::Record)
                .collect(),
        )
        .expect("overlapping background parity facts");
    UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(JournalReadLimits {
            max_bytes: 16 * 1024 * 1024,
            max_entries: 8_192,
        })
        .expect("published complete parity fixture");
    let builder = GlobalReviewPublisher::open_at(&root)
        .expect("builder")
        .expect("candidate");
    let spill = builder.work.0.join("spill");
    drop(storage_file::open_read_write(&spill.join(".private")).expect("private spill"));
    let database = builder.work.0.join("report.duckdb");
    let connection = crate::duckdb_connection::open_background_report(&database, &spill)
        .expect("background engine");
    let actual: (u64,String,String,bool) = connection.query_row("SELECT current_setting('threads')::UBIGINT,current_setting('memory_limit'),current_setting('max_temp_directory_size'),current_setting('autoload_known_extensions')",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).expect("effective budgets");
    assert_eq!(actual, (1, "512.0 MiB".into(), "8.0 GiB".into(), false));
    let expected = builder
        .reader
        .performance_review(TimeRange::all(), &UsageFilter::default())
        .expect("native reference");
    assert_eq!(
        builder
            .reader
            .background_performance_review(connection, &builder.cancellation)
            .expect("background same formulas"),
        expected
    );
    let connection = crate::duckdb_connection::open_background_report(&database, &spill)
        .expect("reopen narrow intervals");
    let narrow: u64 = connection.query_row("SELECT count(*) FROM duckdb_tables() WHERE table_name IN ('background_intervals','background_interval_spans') AND NOT temporary",[],|row|row.get(0)).expect("ordinary background interval storage");
    assert_eq!(narrow, 2);
    drop(connection);
    let connection = crate::duckdb_connection::open_background_report(&database, &spill)
        .expect("reopen private database");
    let tables: u64 = connection.query_row("SELECT count(*) FROM duckdb_tables() WHERE table_name IN ('selected_operations','selected_contributions','selected_operation_statistics','selected_classifications','work_operation_lookup','work_operation_references','work_operation_addresses') AND NOT temporary",[],|row|row.get(0)).expect("spillable ordinary materialization");
    assert_eq!(tables, 7);
    let extreme = format!("SELECT 'extreme' timing_group,1::UBIGINT observation_weight,'tool' kind,
        '{}'::BIGINT started_at_ms,'{}'::BIGINT ended_at_ms,'agent' agent_id,NULL::VARCHAR thread_id,false retired_interval
        UNION ALL SELECT 'retired',3,'tool',NULL,NULL,NULL,NULL,true
        UNION ALL SELECT 'zero',1,'wait',0,0,NULL,NULL,false",
        i64::MIN,i64::MAX);
    let reference =
        crate::report_metrics::grouped_interval_metrics(&connection, &extreme, i64::MIN, i64::MAX)
            .expect("SQL extreme interval reference");
    let streamed = crate::report_metrics::background_grouped_interval_metrics(
        &connection,
        &extreme,
        i64::MIN,
        i64::MAX,
        Some(&builder.cancellation),
    )
    .expect("streamed extreme interval");
    for group in ["extreme", "retired", "zero"] {
        assert_eq!(streamed[group].value(), reference[group].value());
        assert_eq!(
            streamed[group].lifecycle_value(),
            reference[group].lifecycle_value()
        );
    }
    assert_eq!(
        streamed["extreme"].value()["elapsedMs"],
        json!({"measured":u64::MAX,"exact":u64::MAX,"unknown":0})
    );
    assert_eq!(
        streamed["retired"].value()["elapsedMs"],
        json!({"measured":0,"exact":null,"unknown":3})
    );
    assert_eq!(
        streamed["zero"].value()["waitMs"],
        json!({"measured":0,"exact":0,"unknown":0})
    );
    let overflow = format!("WITH source AS ({extreme})
        SELECT * FROM source WHERE timing_group='extreme'
        UNION ALL SELECT * REPLACE ('other-agent' AS agent_id) FROM source WHERE timing_group='extreme'");
    let sql_error =
        crate::report_metrics::grouped_interval_metrics(&connection, &overflow, i64::MIN, i64::MAX)
            .expect_err("two full-range owners must not become an exact saturated active duration");
    assert!(sql_error.to_string().contains("out of range"));
    let streamed_error = crate::report_metrics::background_grouped_interval_metrics(
        &connection,
        &overflow,
        i64::MIN,
        i64::MAX,
        Some(&builder.cancellation),
    )
    .expect_err("streamed active duration must reject the same two-owner overflow");
    assert!(streamed_error.to_string().contains("duration overflow"));
}

#[test]
fn global_analytics_json_limits_are_enforced_without_touching_completed_publication() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    capture(&root, "first", /*at*/ 1_000, /*total*/ 57);
    canonical(&root);
    publish(&root);
    let before = std::fs::read(root.join(PUBLICATION)).expect("completed bytes");
    let candidate = root.join("oversized-candidate.json");
    let mut output = storage_file::create_new(&candidate).expect("private candidate");
    let mut writer = LimitedWriter {
        file: &mut output,
        bytes: 0,
    };
    let block = vec![b' '; 1024 * 1024];
    for _ in 0..32 {
        writer.write_all(&block).expect("within artifact budget");
    }
    assert!(writer.write_all(b" ").is_err());
    assert_eq!(
        std::fs::metadata(candidate).expect("bounded bytes").len(),
        MAX_PUBLICATION_BYTES
    );
    assert_eq!(
        std::fs::read(root.join(PUBLICATION)).expect("old report untouched"),
        before
    );
    let manifest = std::fs::read(root.join("manifest.json")).expect("canonical source bytes");
    let expected = GlobalReviewReader::open_at(&root)
        .expect("valid previous report")
        .expect("publication")
        .report()
        .clone();
    let malformed = root.join(PUBLICATION);
    std::fs::write(
        malformed,
        serde_json::to_vec(&json!({"version":1,"snapshot":{},"report":{}}))
            .expect("malformed payload"),
    )
    .expect("invalid external snapshot");
    assert!(GlobalReviewReader::open_at(&root).is_err());
    publish(&root);
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("repaired reader")
            .expect("canonical rebuild")
            .report(),
        &expected
    );
    assert_eq!(
        std::fs::read(root.join("manifest.json")).expect("canonical source preserved"),
        manifest
    );
    // A parseable envelope with the same source prefix must not suppress repair
    // when critical native fields or the complete outcome row count disappear.
    let complete = std::fs::read(root.join(PUBLICATION)).expect("valid envelope");
    for field in [
        "counts",
        "tokens",
        "timing",
        "cost",
        "contextSize",
        "coverage",
        "outcomes",
    ] {
        let mut damaged: serde_json::Value = serde_json::from_slice(&complete).expect("envelope");
        damaged["report"]
            .as_object_mut()
            .expect("native report")
            .remove(field);
        std::fs::write(
            root.join(PUBLICATION),
            serde_json::to_vec(&damaged).expect("damaged envelope"),
        )
        .expect("parseable incomplete derived snapshot");
        assert!(GlobalReviewReader::open_at(&root).is_err());
        publish(&root);
        assert_eq!(
            GlobalReviewReader::open_at(&root)
                .expect("repaired shape")
                .expect("complete report")
                .report(),
            &expected
        );
    }
    let mut damaged: serde_json::Value = serde_json::from_slice(&complete).expect("envelope");
    damaged["report"]["outcomes"]["totalRows"] = json!(999);
    std::fs::write(
        root.join(PUBLICATION),
        serde_json::to_vec(&damaged).expect("damaged row count"),
    )
    .expect("inconsistent outcome rows");
    assert!(GlobalReviewReader::open_at(&root).is_err());
    publish(&root);
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("repaired row count")
            .expect("complete report")
            .report(),
        &expected
    );
    assert_eq!(
        std::fs::read(root.join("manifest.json"))
            .expect("canonical source preserved after all repairs"),
        manifest
    );
    let mut retired: serde_json::Value =
        serde_json::from_slice(&complete).expect("completed envelope");
    retired
        .as_object_mut()
        .expect("envelope object")
        .remove("source_manifest_identity");
    std::fs::write(
        root.join(PUBLICATION),
        serde_json::to_vec(&retired).expect("unshipped prior envelope"),
    )
    .expect("derived envelope without source identity");
    assert!(GlobalReviewReader::open_at(&root).is_err());
    publish(&root);
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("rebuilt current envelope")
            .expect("canonical result")
            .report(),
        &expected
    );
}

#[test]
fn global_analytics_preparation_defers_large_footer_plans_without_starting_report_engine() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    capture(&root, "first", /*at*/ 1_000, /*total*/ 57);
    canonical(&root);
    publish(&root);
    let before = std::fs::read(root.join(PUBLICATION)).expect("completed report");
    capture(&root, "second", /*at*/ 2_000, /*total*/ 61);
    canonical(&root);
    let manifest = crate::ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("source");
    let descriptor = crate::review_projection::load_review_projection_shared(&root, &manifest)
        .expect("review descriptor")
        .expect("published review");
    let operation = root.join("segments").join(&descriptor.partitions[0].file);
    let original = std::fs::read(&operation).expect("queried operation bytes");
    {
        // The footer length alone must withhold admission before any metadata
        // decode, even when the oversized contents cannot be parsed by DuckDB.
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&operation)
            .expect("oversized queried operation metadata");
        output
            .set_len(FOOTER_BUDGET + 13)
            .expect("sparse oversized metadata");
        output.write_all(b"PAR1").expect("Parquet prefix");
        output.seek(SeekFrom::End(-8)).expect("footer");
        output
            .write_all(&((FOOTER_BUDGET + 1) as u32).to_le_bytes())
            .expect("metadata length");
        output.write_all(b"PAR1").expect("Parquet suffix");
    }
    {
        let mut builder = GlobalReviewPublisher::open_at(&root)
            .expect("builder")
            .expect("new source prefix");
        assert_eq!(
            builder.publish().expect("bounded preparation result"),
            GlobalReviewPublication::AwaitingCompaction
        );
        assert!(!builder.work.0.join("report.duckdb").exists());
        assert_eq!(
            std::fs::read(root.join(PUBLICATION)).expect("old complete result"),
            before
        );
    }
    std::fs::write(operation, original).expect("restore queried operation source");
    publish(&root);
    assert_eq!(
        GlobalReviewReader::open_at(&root)
            .expect("prepared reader")
            .expect("complete")
            .snapshot()
            .included_journal_watermark,
        2
    );
}

#[test]
fn global_analytics_cancelled_candidate_never_replaces_completed_publication() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    capture(&root, "first", /*at*/ 1_000, /*total*/ 57);
    canonical(&root);
    publish(&root);
    let previous = std::fs::read(root.join(PUBLICATION)).expect("completed report");
    capture(&root, "second", /*at*/ 2_000, /*total*/ 61);
    let cancellation = Arc::new(GlobalReviewCancellation::default());
    {
        let mut builder =
            GlobalReviewPublisher::open_cancellable(root.clone(), Arc::clone(&cancellation))
                .expect("cancellable builder")
                .expect("new source");
        cancellation.cancel();
        assert!(builder.publish().is_err());
    }
    assert_eq!(
        std::fs::read(root.join(PUBLICATION)).expect("completed report retained"),
        previous
    );
    assert!(!std::fs::read_dir(&root).expect("root").any(|entry| {
        entry
            .expect("entry")
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("global-review-work-"))
    }));
    let candidate = root.join("ordered-candidate.json");
    std::fs::write(&candidate, b"must not replace completed report")
        .expect("cancel-first candidate");
    let cancel_first = GlobalReviewCancellation::default();
    let ready = std::sync::Barrier::new(/*n*/ 2);
    std::thread::scope(|scope| {
        let cancelled = scope.spawn(|| {
            cancel_first.cancel();
            ready.wait();
        });
        ready.wait(); // Cancellation has completed before commit admission.
        assert!(
            cancel_first
                .commit(|| {
                    std::fs::rename(&candidate, root.join(PUBLICATION))?;
                    Ok(())
                })
                .is_err()
        );
        cancelled.join().expect("cancel-first worker");
    });
    assert_eq!(
        std::fs::read(root.join(PUBLICATION)).expect("cancel-first previous bytes"),
        previous
    );
    let mut committed = previous;
    committed.push(b'\n');
    std::fs::write(&candidate, &committed).expect("commit-first candidate");
    let commit_first = GlobalReviewCancellation::default();
    let renamed = std::sync::Barrier::new(/*n*/ 2);
    std::thread::scope(|scope| {
        let published = scope.spawn(|| {
            commit_first.commit(|| {
                std::fs::rename(&candidate, root.join(PUBLICATION))?;
                renamed.wait(); // Cancellation races only after this rename completed.
                Ok(())
            })
        });
        renamed.wait();
        commit_first.cancel();
        published
            .join()
            .expect("commit-first worker")
            .expect("earlier commit remains successful");
    });
    assert_eq!(
        std::fs::read(root.join(PUBLICATION)).expect("commit-first completed bytes"),
        committed
    );
    assert!(commit_first.check().is_err());
}
