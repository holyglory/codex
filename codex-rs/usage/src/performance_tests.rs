//! Explicit acceptance benchmark. No scale or latency work runs in ordinary tests.
//! Set CODEX_USAGE_SCALE_ARTIFACT_ROOT to an approved disk-backed directory; the
//! fixture and receipts are retained for the governed run's subsequent cleanup.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;

use crate::review_projection::load_review_projection;
use crate::*;

#[path = "performance_fixture.rs"]
mod fixture;

const LIMITS: JournalReadLimits = JournalReadLimits {
    // The acceptance fixture uses a larger bounded publication window than
    // ordinary foreground recovery so the 15M fact run measures throughput
    // instead of spending hours on per-16K manifest commits.
    max_bytes: 1024 * 1024 * 1024,
    max_entries: 1_048_576,
};
const RECOVERY_TAIL: u64 = 8_192;
const HELPER: &str = "performance_tests::fresh_process_recovery_helper";

#[derive(Default, Serialize)]
struct Publication {
    batches: u64,
    maximum_batch_ms: f64,
    maximum_batch_entries: u64,
    maximum_batch_bytes: u64,
    elapsed_ms: f64,
}

impl Publication {
    fn observe(&mut self, before: &UsageManifest, after: &UsageManifest, elapsed: Duration) {
        let entries = after.watermark - before.watermark;
        let bytes = after.journal_byte_offset - before.journal_byte_offset;
        assert!(
            entries <= LIMITS.max_entries as u64 && bytes <= LIMITS.max_bytes,
            "publication exceeded its input budget"
        );
        self.batches += 1;
        self.maximum_batch_ms = self.maximum_batch_ms.max(milliseconds(elapsed));
        self.maximum_batch_entries = self.maximum_batch_entries.max(entries);
        self.maximum_batch_bytes = self.maximum_batch_bytes.max(bytes);
    }
}

fn publish_through(root: &Path, target: JournalCheckpoint) -> Publication {
    let started = Instant::now();
    let mut publisher = UsagePublisher::open_at(root).expect("publisher metadata");
    let mut manifest = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("manifest");
    let mut measurements = Publication::default();
    while manifest.watermark < target.sequence {
        let step = Instant::now();
        let next = publisher
            .publish_batch(LIMITS)
            .expect("bounded publication");
        measurements.observe(&manifest, &next, step.elapsed());
        assert!(
            next.watermark > manifest.watermark,
            "publisher made no progress toward captured target"
        );
        manifest = next;
        if measurements.batches % 16 == 0 {
            eprintln!(
                "usage-scale {}",
                json!({"stage":"publication","watermark":manifest.watermark,"target":target.sequence,"maximumBatchMs":measurements.maximum_batch_ms})
            );
        }
    }
    measurements.elapsed_ms = milliseconds(started.elapsed());
    measurements
}

fn append_buffer(journal: &UsageJournal, buffer: &mut Vec<JournalPayload>) {
    if !buffer.is_empty() {
        journal
            .append_batch(std::mem::replace(
                buffer,
                Vec::with_capacity(LIMITS.max_entries),
            ))
            .expect("durable batch capture");
    }
}

fn capture_fixture(journal: &UsageJournal) -> serde_json::Value {
    let started = Instant::now();
    let mut buffer = Vec::with_capacity(LIMITS.max_entries);
    let mut counts = BTreeMap::<String, u64>::new();
    for round in 0..fixture::ROUNDS {
        for record in fixture::round_records(round) {
            *counts
                .entry(record.event.as_ref().expect("fact kind").clone())
                .or_default() += 1;
            buffer.push(JournalPayload::Record(record));
            if buffer.len() == LIMITS.max_entries {
                append_buffer(journal, &mut buffer);
            }
        }
        if round % 65_536 == 0 {
            eprintln!(
                "usage-scale {}",
                json!({"stage":"capture","rounds":round,"targetRounds":fixture::ROUNDS})
            );
        }
    }
    append_buffer(journal, &mut buffer);
    assert_eq!(counts.get("tool_started"), Some(&3_000_000));
    assert_eq!(counts.get("tool_finished"), Some(&3_000_000));
    assert_eq!(counts.get("model_request_started"), Some(&1_000_000));
    assert_eq!(counts.get("model_request_finished"), Some(&1_000_000));
    assert_eq!(counts.get("token_observation"), Some(&7_000_000));
    assert_eq!(
        journal.checkpoint().expect("capture boundary").sequence,
        fixture::FACTS
    );
    json!({"counts":counts,"elapsedMs":milliseconds(started.elapsed())})
}

#[derive(Default, Serialize)]
struct ConcurrentCapture {
    facts: u64,
    batches: u64,
    maximum_batch_ms: f64,
    elapsed_ms: f64,
}

fn capture_live(
    root: &Path,
    stop: mpsc::Receiver<()>,
    wake: mpsc::SyncSender<()>,
    ready: mpsc::SyncSender<()>,
    progress: Arc<AtomicU64>,
) -> ConcurrentCapture {
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let started = Instant::now();
    let mut measurements = ConcurrentCapture::default();
    for _ in 0..512 {
        let start = measurements.facts;
        let step = Instant::now();
        journal
            .append_batch(
                (start..start + 128)
                    .map(|index| JournalPayload::Record(fixture::live_record(index)))
                    .collect(),
            )
            .expect("capture alongside reports/publication");
        measurements.facts += 128;
        progress.store(measurements.facts, Ordering::Release);
        measurements.batches += 1;
        measurements.maximum_batch_ms = measurements
            .maximum_batch_ms
            .max(milliseconds(step.elapsed()));
        let _ = wake.try_send(());
        if measurements.batches == 1 {
            ready.send(()).expect("capture ready");
            continue;
        }
        match stop.recv_timeout(Duration::from_millis(10)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    measurements.elapsed_ms = milliseconds(started.elapsed());
    measurements
}

fn publish_live(root: &Path, stop: mpsc::Receiver<()>, wake: mpsc::Receiver<()>) -> Publication {
    let started = Instant::now();
    let mut publisher = UsagePublisher::open_at(root).expect("live publisher");
    let mut manifest = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("live manifest");
    let mut measurements = Publication::default();
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        match wake.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let step = Instant::now();
        let next = publisher
            .publish_batch(LIMITS)
            .expect("live bounded publication");
        measurements.observe(&manifest, &next, step.elapsed());
        manifest = next;
    }
    measurements.elapsed_ms = milliseconds(started.elapsed());
    measurements
}

fn report_measurements(
    root: &Path,
    warm: &UsageReader,
    expected: &fixture::Expectations,
    failures: &mut Vec<String>,
) -> serde_json::Value {
    let start = Instant::now();
    let current = UsageReader::open_at(root)
        .expect("current-chat reader open")
        .summary(
            TimeRange::all(),
            &UsageFilter::default().for_thread(fixture::thread_id(fixture::CURRENT_THREAD)),
        )
        .expect("current-chat summary");
    let current_output = serde_json::to_vec(&current).expect("current summary output");
    let current_elapsed = start.elapsed();
    target(
        "current-chat open plus summary",
        current_elapsed,
        Duration::from_millis(200),
        failures,
    );
    if !summary_matches(
        &current,
        &expected.current_thread,
        &expected.current_thread_counts,
    ) {
        failures
            .push("current-chat exact token totals or canonical operation counts differ".into());
    }

    let start = Instant::now();
    let global = warm
        .summary(TimeRange::all(), &UsageFilter::default())
        .expect("warm global summary");
    let global_output = serde_json::to_vec(&global).expect("global summary output");
    let global_elapsed = start.elapsed();
    target(
        "warm global summary",
        global_elapsed,
        Duration::from_secs(1),
        failures,
    );
    if !summary_matches(&global, &expected.global, &expected.global_counts) {
        failures.push("warm global exact token totals or canonical operation counts differ".into());
    }

    let filter = UsageFilter::default().for_repository(fixture::review_repository());
    let start = Instant::now();
    let repository_reader = UsageReader::open_at(root).expect("repository reader open");
    let review = repository_reader
        .performance_review(fixture::review_range(), &filter)
        .expect("finite repository performance review");
    let output = serde_json::to_vec(&review).expect("serialize performance review");
    let review_elapsed = start.elapsed();
    target(
        "finite repository open plus full performance review",
        review_elapsed,
        Duration::from_secs(2),
        failures,
    );
    if review["counts"]["operations"].as_u64() != Some(expected.finite_repository_counts.operations)
        || review["counts"]["tools"].as_u64() != Some(expected.finite_repository_counts.tools)
        || review["counts"]["model_requests"].as_u64()
            != Some(expected.finite_repository_counts.model_requests)
        || review["counts"]["unfinished"].as_u64() != Some(0)
        || review["counts"]["unknown_intervals"].as_u64() != Some(0)
        || review["coverage"]["deduplicatedOperations"].as_u64()
            != Some(expected.finite_repository_counts.operations)
        || review["coverage"]["deduplicatedTokenObservations"].as_u64()
            != Some(expected.finite_token_observations)
    {
        failures.push(
            "finite performance-review canonical operation or observation counts differ".into(),
        );
    }
    let provider_total = review["tokens"]
        .as_array()
        .and_then(|tokens| {
            tokens
                .iter()
                .find(|token| token["category"] == "total_tokens")
        })
        .and_then(|token| token["exactTokens"].as_i64());
    if provider_total
        != Some(expected.finite_repository.input_tokens + expected.finite_repository.output_tokens)
    {
        failures.push("finite performance-review exact disjoint provider totals differ".into());
    }
    let page_start = Instant::now();
    let page = repository_reader
        .operation_page(
            fixture::review_range(),
            &filter,
            OperationSelection::All,
            /*after*/ None,
            /*limit*/ 50,
        )
        .expect("finite operation evidence page");
    let page_elapsed = page_start.elapsed();
    if page
        .data
        .iter()
        .map(|operation| operation.operation_id.clone())
        .collect::<Vec<_>>()
        != expected.first_operation_ids
        || page.next_cursor.is_none()
    {
        failures.push("finite operation evidence page or continuation differs".into());
    }
    let totals_start = Instant::now();
    let repository_totals = repository_reader
        .totals(fixture::review_range(), &filter)
        .expect("finite repository exact totals");
    let totals_elapsed = totals_start.elapsed();
    if repository_totals != expected.finite_repository {
        failures.push("finite repository exact totals differ".into());
    }
    json!({"currentChatMs":milliseconds(current_elapsed),"warmGlobalMs":milliseconds(global_elapsed),"finiteReviewMs":milliseconds(review_elapsed),"finiteEvidencePageMs":milliseconds(page_elapsed),"finiteTotalsMs":milliseconds(totals_elapsed),"currentChatSummary":current,"warmGlobalSummary":global,"currentChatOutputBytes":current_output.len(),"warmGlobalOutputBytes":global_output.len(),"finiteTotals":repository_totals,"review":review,"reviewRows":page.data.len(),"reviewPage":page.data,"reviewOutputBytes":output.len(),"warmSnapshotGeneration":warm.manifest().snapshot_generation})
}

#[test]
#[ignore = "15M-fact acceptance benchmark; requires governed artifact storage"]
fn synthetic_scale_acceptance() {
    let base = std::env::var_os("CODEX_USAGE_SCALE_ARTIFACT_ROOT")
        .expect("set CODEX_USAGE_SCALE_ARTIFACT_ROOT to approved disk-backed storage (allow 30GB)");
    let artifact = tempfile::Builder::new()
        .prefix("codex-usage-scale-")
        .tempdir_in(base)
        .expect("artifact directory")
        .keep();
    eprintln!(
        "usage-scale {}",
        json!({"stage":"started","artifact":artifact,"facts":fixture::FACTS,"debugAssertions":cfg!(debug_assertions)})
    );
    let codex_home = artifact.join("codex-home");
    let root = codex_home.join(USAGE_DIRECTORY_NAME);
    fs::create_dir_all(&root).expect("isolated store");
    let expected = fixture::Expectations::calculate();
    let expectation_path = artifact.join("expectations.json");
    fs::write(
        &expectation_path,
        serde_json::to_vec(&expected).expect("expectations JSON"),
    )
    .expect("expectation artifact");
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let capture = capture_fixture(&journal);
    fs::write(
        artifact.join("capture.json"),
        serde_json::to_vec_pretty(&capture).expect("capture JSON"),
    )
    .expect("capture evidence before publication");
    let initial_target = journal.checkpoint().expect("captured target");
    let publication = publish_through(&root, initial_target);
    let warm = UsageReader::open_at(&root).expect("pin fully published warm snapshot");

    let (stop_capture, capture_stop) = mpsc::channel();
    let (stop_publication, publication_stop) = mpsc::channel();
    let (wake, wakes) = mpsc::sync_channel(1);
    let (ready, readiness) = mpsc::sync_channel(0);
    let capture_progress = Arc::new(AtomicU64::default());
    let progress = Arc::clone(&capture_progress);
    let capture_root = root.clone();
    let capture_worker =
        thread::spawn(move || capture_live(&capture_root, capture_stop, wake, ready, progress));
    let publisher_root = root.clone();
    let publisher_worker =
        thread::spawn(move || publish_live(&publisher_root, publication_stop, wakes));
    readiness.recv().expect("concurrent capture started");
    let capture_before_reports = capture_progress.load(Ordering::Acquire);
    let mut failures = Vec::new();
    let queries = report_measurements(&root, &warm, &expected, &mut failures);
    let captured_during_reports = capture_progress.load(Ordering::Acquire) - capture_before_reports;
    if captured_during_reports == 0 {
        failures.push("capture made no durable progress during the report sequence".into());
    }
    let _ = stop_capture.send(());
    let concurrent_capture = capture_worker.join().expect("capture worker completed");
    let _ = stop_publication.send(());
    let concurrent_publication = publisher_worker.join().expect("publisher worker completed");
    if concurrent_capture.facts == 0 {
        failures.push("no concurrent capture progressed".into());
    }
    // Isolate the planned 8192-fact startup workload from any separately
    // measured concurrent-publication backlog; retain that backlog and drain
    // cost as evidence rather than silently folding it into another target.
    let concurrent_target = journal.checkpoint().expect("concurrent captured head");
    let concurrent_published = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("concurrent published head");
    let concurrent_pending = concurrent_target.sequence - concurrent_published.watermark;
    let concurrent_drain = publish_through(&root, concurrent_target);

    // Leave a real complete tail larger than the foreground overlay budget.
    let mut recovery_tail = Vec::with_capacity(RECOVERY_TAIL as usize);
    for index in concurrent_capture.facts..concurrent_capture.facts + RECOVERY_TAIL {
        recovery_tail.push(JournalPayload::Record(fixture::live_record(index)));
    }
    journal
        .append_batch(recovery_tail)
        .expect("unpublished recovery tail");
    let head = journal.checkpoint().expect("recovery head");
    assert_eq!(
        head.sequence,
        fixture::FACTS + concurrent_capture.facts + RECOVERY_TAIL
    );
    let recovery_path = artifact.join("fresh-process.json");
    let start = Instant::now();
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", HELPER, "--ignored", "--nocapture"])
        .env("CODEX_USAGE_SCALE_HOME", &codex_home)
        .env("CODEX_USAGE_SCALE_EXPECTATIONS", &expectation_path)
        .env("CODEX_USAGE_SCALE_RECOVERY_RECEIPT", &recovery_path)
        .output()
        .expect("fresh-process recovery");
    let recovery_elapsed = start.elapsed();
    fs::write(artifact.join("fresh-process.stdout"), &child.stdout).expect("child stdout evidence");
    fs::write(artifact.join("fresh-process.stderr"), &child.stderr).expect("child stderr evidence");
    target(
        "fresh-process startup/recovery",
        recovery_elapsed,
        Duration::from_secs(2),
        &mut failures,
    );
    if !child.status.success() {
        failures.push("fresh-process helper failed; inspect retained child evidence".into());
    }
    let recovery = fs::read(&recovery_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_else(|| json!({"missingReceipt":true}));
    if recovery["exactTotalsMatch"].as_bool() != Some(true)
        || recovery["exactOperationCountsMatch"].as_bool() != Some(true)
        || recovery["fullyRecovered"].as_bool() != Some(true)
        || recovery["publishedWatermark"].as_u64() != Some(head.sequence)
        || recovery["publishedByteOffset"].as_u64() != Some(head.byte_offset)
        || recovery["pendingEntries"].as_u64() != Some(0)
        || recovery["head"]["sequence"].as_u64() != Some(head.sequence)
    {
        failures
            .push("fresh-process receipt is missing, invalid, or for a different fixture".into());
    }
    let final_publication = publish_through(&root, head);
    let final_reader = UsageReader::open_at(&root).expect("fully recovered report");
    let mut final_expected = expected.global;
    final_expected.records = head.sequence;
    let final_totals = final_reader
        .totals(TimeRange::all(), &UsageFilter::default())
        .expect("final durable exact totals");
    if final_totals != final_expected || final_reader.pending_entries() != 0 {
        failures.push("full recovery lost or double-counted facts".into());
    }
    let final_counts =
        published_counts(&root, final_reader.manifest()).expect("final published operation counts");
    if final_counts != recovery_counts(&expected.global_counts, head.sequence) {
        failures.push("full recovery lost or double-counted canonical operations".into());
    }
    let receipt = json!({"schemaVersion":1,"debugAssertions":cfg!(debug_assertions),"baselineFacts":fixture::FACTS,"canonicalOperations":4_000_000,"toolOperations":3_000_000,"modelRequests":1_000_000,"toolLifecycleFacts":6_000_000,"modelRequestLifecycleFacts":2_000_000,"tokenObservations":7_000_000,"journalBytes":head.byte_offset,"capture":capture,"publication":publication,"queries":queries,"capturedDuringReports":captured_during_reports,"concurrentCapture":concurrent_capture,"concurrentPublication":concurrent_publication,"concurrentPendingEntries":concurrent_pending,"concurrentDrain":concurrent_drain,"recoveryTailEntries":RECOVERY_TAIL,"freshProcessMs":milliseconds(recovery_elapsed),"freshProcess":recovery,"finalPublication":final_publication,"finalTotals":final_totals,"finalOperationCounts":final_counts,"finalSnapshotGeneration":final_reader.manifest().snapshot_generation,"segmentFiles":final_reader.manifest().segment_files.len(),"failures":failures});
    let receipt_path = artifact.join("acceptance.json");
    fs::write(
        &receipt_path,
        serde_json::to_vec_pretty(&receipt).expect("receipt JSON"),
    )
    .expect("acceptance receipt");
    eprintln!(
        "usage-scale {}",
        json!({"stage":"finished","receipt":receipt_path,"failedChecks":failures.len()})
    );
    assert!(
        failures.is_empty(),
        "scale acceptance failed; inspect {}",
        receipt_path.display()
    );
}

#[test]
#[ignore = "child-process fixture for synthetic_scale_acceptance"]
fn fresh_process_recovery_helper() {
    let codex_home =
        PathBuf::from(std::env::var_os("CODEX_USAGE_SCALE_HOME").expect("child store"));
    let expected_path =
        std::env::var_os("CODEX_USAGE_SCALE_EXPECTATIONS").expect("child expectations");
    let receipt_path =
        std::env::var_os("CODEX_USAGE_SCALE_RECOVERY_RECEIPT").expect("child receipt");
    let expected: fixture::Expectations =
        serde_json::from_slice(&fs::read(expected_path).expect("read expectations"))
            .expect("decode expectations");
    let started = Instant::now();
    let root = codex_home.join(USAGE_DIRECTORY_NAME);
    let head = UsageJournal::new(root.join("events.jsonl"))
        .checkpoint()
        .expect("captured acknowledged recovery target");
    let runtime =
        UsagePublisherRuntime::start(&codex_home).expect("independent recovery worker starts");
    let immediate = UsageReader::open(&codex_home).expect("immediate consistent snapshot");
    let immediate_summary = immediate
        .summary(TimeRange::all(), &UsageFilter::default())
        .expect("immediate consistent snapshot summary");
    let immediate_elapsed = started.elapsed();
    let manifests = ManifestStore::new(root.join("manifest.json"));
    let mut published = manifests.load().expect("recovery progress metadata");
    let mut polls = 0;
    while published.watermark < head.sequence && started.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(20));
        published = manifests.load().expect("published recovery progress");
        polls += 1;
    }
    let mut wanted = expected.global;
    wanted.records = head.sequence;
    let publication_complete =
        published.watermark == head.sequence && published.journal_byte_offset == head.byte_offset;
    let wanted_counts = recovery_counts(&expected.global_counts, head.sequence);
    let (actual, counts, pending_entries, totals_match, counts_match) = if publication_complete {
        let recovered = UsageReader::open(&codex_home).expect("fully recovered reader");
        let actual = recovered
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("all acknowledged recovered totals");
        let counts = published_counts(&root, recovered.manifest())
            .expect("all recovered canonical operation counts");
        let summary = recovered
            .summary(TimeRange::all(), &UsageFilter::default())
            .expect("fully recovered foreground summary");
        let totals_match = actual == wanted;
        let counts_match =
            counts == wanted_counts && summary_matches(&summary, &wanted, &wanted_counts);
        (
            Some(actual),
            Some(counts),
            recovered.pending_entries(),
            totals_match,
            counts_match,
        )
    } else {
        (
            None,
            None,
            head.sequence.saturating_sub(published.watermark),
            false,
            false,
        )
    };
    let elapsed = started.elapsed();
    let fully_recovered = publication_complete
        && totals_match
        && counts_match
        && pending_entries == 0
        && elapsed < Duration::from_secs(2);
    fs::write(receipt_path, serde_json::to_vec_pretty(&json!({"fullRecoveryMs":milliseconds(elapsed),"head":head,"publishedWatermark":published.watermark,"publishedByteOffset":published.journal_byte_offset,"pendingEntries":pending_entries,"fullyRecovered":fully_recovered,"totals":actual,"operationCounts":counts,"exactTotalsMatch":totals_match,"exactOperationCountsMatch":counts_match,"manifestPolls":polls,"immediateSnapshot":{"openAndSummaryMs":milliseconds(immediate_elapsed),"watermark":immediate.manifest().watermark,"pendingEntries":immediate.pending_entries(),"summary":immediate_summary}})).expect("recovery JSON")).expect("recovery receipt");
    drop(runtime);
    assert!(
        fully_recovered,
        "fresh process did not recover the captured tail and exact acknowledged totals/counts within 2s"
    );
}

fn published_counts(root: &Path, manifest: &UsageManifest) -> Result<ReviewCounts> {
    let descriptor = load_review_projection(root, manifest)?
        .ok_or_else(|| StorageError::InvalidData("missing operation projection".into()))?;
    if descriptor.complete_from_sequence != 1 {
        return Err(StorageError::InvalidData(
            "partial operation projection".into(),
        ));
    }
    read_review_counts(root, &descriptor, &ReviewCountScope::Global)
}

fn recovery_counts(baseline: &ReviewCounts, head: u64) -> ReviewCounts {
    let additional = head
        .checked_sub(fixture::FACTS)
        .expect("captured full baseline");
    baseline
        .checked_add(&ReviewCounts {
            operations: additional,
            tools: additional,
            tool_completed: additional,
            ..ReviewCounts::default()
        })
        .expect("recovery count oracle")
}

fn summary_matches(
    summary: &serde_json::Value,
    totals: &UsageTotals,
    counts: &ReviewCounts,
) -> bool {
    let expected = json!({"inputTokens":totals.input_tokens,"cachedInputTokens":totals.cached_input_tokens,
        "outputTokens":totals.output_tokens,"reasoningOutputTokens":totals.reasoning_output_tokens,"records":totals.records});
    let expected_counts = json!({"operations":counts.operations,"modelRequests":counts.model_requests,
        "tools":counts.tools,"warmups":counts.warmups,"unfinished":counts.unfinished,
        "unknownIntervals":counts.unknown_intervals,"unclassified":counts.unclassified,
        "missingRepository":counts.missing_repository});
    expected
        .as_object()
        .expect("token oracle")
        .iter()
        .all(|(field, value)| summary[field] == *value)
        && expected_counts
            .as_object()
            .expect("operation oracle")
            .iter()
            .all(|(field, value)| summary["counts"][field] == *value)
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn target(name: &str, elapsed: Duration, limit: Duration, failures: &mut Vec<String>) {
    if elapsed >= limit {
        failures.push(format!(
            "{name}: {:.3}ms exceeds {:.3}ms",
            milliseconds(elapsed),
            milliseconds(limit)
        ));
    }
}
