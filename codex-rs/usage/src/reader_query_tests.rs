//! Reference the full published raw index without generation pruning.

use super::*;

pub(crate) fn full_index_totals(
    reader: &UsageReader,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<UsageTotals> {
    let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
    let memberships = if reader.manifest.membership_files.is_empty() {
        reader.manifest.membership_file.as_slice()
    } else {
        &reader.manifest.membership_files
    };
    let files = if memberships.is_empty() {
        reader.manifest.segment_files.clone()
    } else {
        indexed_files(&connection, reader, range, filter, memberships)?
    };
    SnapshotQuery::from_files(connection, reader, range, filter, &files)?.totals()
}

pub(crate) fn assert_generation_bounds(reader: &UsageReader) -> Result<()> {
    let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
    for file in &reader.manifest.membership_files {
        let Some(bound) = crate::detail_retention::file_generation(file, "membership-") else {
            continue;
        };
        let source = parquet_source(
            &reader.root.join("segments"),
            std::slice::from_ref(file),
            &[("segment_file", "VARCHAR")],
        )?;
        let mut statement = connection
            .prepare(&format!("SELECT DISTINCT segment_file FROM ({source})"))
            .map_err(query_error)?;
        for row in statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(query_error)?
        {
            if let Some(origin) =
                crate::detail_retention::file_generation(&row.map_err(query_error)?, "segment-")
            {
                assert!(
                    origin <= bound,
                    "membership upper bound precedes a referenced raw origin"
                );
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "read-only reference check against an explicitly selected private usage snapshot"]
fn real_window_matches_the_full_raw_index() -> Result<()> {
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(
        std::env::var_os("USAGE_WINDOW_PROBE_INPUT").expect("private probe input"),
    )?)?;
    let reader = UsageReader::open(std::path::PathBuf::from(
        input["codex_home"].as_str().expect("home"),
    ))?;
    let range = TimeRange::from_millis(
        input["start_ms"].as_i64().expect("start"),
        input["end_ms"].as_i64().expect("end"),
    )?;
    let filter =
        UsageFilter::default().for_repository(input["repository_id"].as_str().expect("scope"));
    let request = std::time::Instant::now();
    let window = reader.prepare_window(range, &filter)?;
    let summary = window.summary()?;
    let _ = window.performance_review()?;
    let _ = window.bound_work_effort(&Default::default())?;
    let prepared_ms = request.elapsed().as_millis();
    let prepared_peak = peak_rss_kib();
    eprintln!(
        "{}",
        serde_json::json!({"phase":"prepared_request","elapsed_ms":prepared_ms,"peak_rss_kib":prepared_peak})
    );
    let started = std::time::Instant::now();
    let actual = reader.totals(range, &filter)?;
    let selected_index_ms = started.elapsed().as_millis();
    let selected_peak = peak_rss_kib();
    eprintln!(
        "{}",
        serde_json::json!({"phase":"raw_fallback","elapsed_ms":selected_index_ms,
        "request_upper_bound_ms":prepared_ms+selected_index_ms,"peak_rss_kib":selected_peak})
    );
    let started = std::time::Instant::now();
    let expected = full_index_totals(&reader, range, &filter)?;
    let full_index_ms = started.elapsed().as_millis();
    pretty_assertions::assert_eq!(actual, expected);
    pretty_assertions::assert_eq!(
        serde_json::json!([
            summary["inputTokens"],
            summary["cachedInputTokens"],
            summary["outputTokens"],
            summary["reasoningOutputTokens"],
            summary["records"]
        ]),
        serde_json::json!([
            expected.input_tokens,
            expected.cached_input_tokens,
            expected.output_tokens,
            expected.reasoning_output_tokens,
            expected.records
        ])
    );
    eprintln!(
        "{}",
        serde_json::json!({"phase":"same_pin_full_index_reference","full_index_ms":full_index_ms,
            "selected_index_ms":selected_index_ms,"prepared_request_ms":prepared_ms,
            "fallback_request_upper_bound_ms":prepared_ms+selected_index_ms,
            "prepared_peak_rss_kib":prepared_peak,"fallback_peak_rss_kib":selected_peak,
            "full_index_peak_rss_kib":peak_rss_kib(),"equal":true,"summary_components_equal":true})
    );
    assert!(
        prepared_ms + selected_index_ms <= 30_000,
        "existing 30-second consumer budget exceeded"
    );
    Ok(())
}

fn peak_rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|line| line.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[test]
fn opaque_raw_and_membership_names_keep_the_full_index_conservative() -> Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let mut store = crate::UsageStore::open(home.path())?;
    for hour in 0..6 {
        store.append_record(
            crate::UsageRecord::new_millis(
                format!("old-{hour}"),
                hour * 3_600_000,
                /*input_tokens*/ 1,
                /*output_tokens*/ 0,
            )
            .with_repository("repo-a"),
        )?;
    }
    let limits = crate::JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    };
    crate::UsagePublisher::open_at(&root)?.publish_batch(limits)?;
    store.append_record(
        crate::UsageRecord::new_millis(
            "selected",
            10 * 3_600_000 + 1_000,
            /*input_tokens*/ 7,
            /*output_tokens*/ 2,
        )
        .with_repository("repo-a"),
    )?;
    crate::UsagePublisher::open_at(&root)?.publish_batch(limits)?;
    store.append_record(
        crate::UsageRecord::new_millis(
            "other",
            10 * 3_600_000 + 1_000,
            /*input_tokens*/ 13,
            /*output_tokens*/ 3,
        )
        .with_repository("repo-b"),
    )?;
    crate::UsagePublisher::open_at(&root)?.publish_batch(limits)?;
    let manifest_store = crate::ManifestStore::new(root.join("manifest.json"));
    let normal = manifest_store.load()?;
    let raw = normal
        .segment_files
        .iter()
        .find(|file| crate::detail_retention::file_generation(file, "segment-") == Some(2))
        .expect("selected batch")
        .clone();
    let membership = normal
        .membership_files
        .iter()
        .find(|file| crate::detail_retention::file_generation(file, "membership-") == Some(2))
        .expect("selected index")
        .clone();
    let directory = root.join("segments");
    std::fs::copy(
        directory.join(&raw),
        directory.join("legacy-opaque.parquet"),
    )?;
    let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
    let source = parquet_source(
        &directory,
        std::slice::from_ref(&membership),
        &[
            ("dimension_kind", "VARCHAR"),
            ("dimension_id", "VARCHAR"),
            ("hour_start", "BIGINT"),
            ("segment_file", "VARCHAR"),
        ],
    )?;
    let replacement = "membership-00000000000000000002_legacy.parquet";
    crate::review_storage::write_query(
        &connection,
        &root,
        replacement,
        &format!("SELECT * REPLACE ('legacy-opaque.parquet' AS segment_file) FROM ({source})"),
    )?;
    let mut legacy = normal.clone();
    for file in &mut legacy.segment_files {
        if *file == raw {
            *file = "legacy-opaque.parquet".into();
        }
    }
    for file in &mut legacy.membership_files {
        if *file == membership {
            *file = replacement.into();
        }
    }
    manifest_store.save(&legacy)?;
    let range = TimeRange::from_millis(10 * 3_600_000, 11 * 3_600_000)?;
    let filter = UsageFilter::default().for_repository("repo-a");
    let expected = UsageTotals {
        input_tokens: 7,
        output_tokens: 2,
        records: 1,
        ..Default::default()
    };
    let reader = UsageReader::open(home.path())?;
    pretty_assertions::assert_eq!(reader.totals(range, &filter)?, expected);
    pretty_assertions::assert_eq!(full_index_totals(&reader, range, &filter)?, expected);
    std::fs::copy(
        directory.join(&membership),
        directory.join("membership-opaque.parquet"),
    )?;
    let mut legacy = normal;
    for file in &mut legacy.membership_files {
        if *file == membership {
            *file = "membership-opaque.parquet".into();
        }
    }
    manifest_store.save(&legacy)?;
    let reader = UsageReader::open(home.path())?;
    pretty_assertions::assert_eq!(reader.totals(range, &filter)?, expected);
    pretty_assertions::assert_eq!(full_index_totals(&reader, range, &filter)?, expected);
    Ok(())
}
