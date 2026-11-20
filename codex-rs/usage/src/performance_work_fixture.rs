//! Sparse switch schedule and independent arithmetic acceptance oracles.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use super::fixture;
use crate::*;

const PROJECT: &str = "benchmark-project";
const CYCLE: u64 = fixture::THREADS * 8;
const THREADS: [u64; 3] = [
    fixture::CURRENT_THREAD,
    fixture::CURRENT_THREAD + 256,
    fixture::CURRENT_THREAD + 61 * 256,
];

/// Twelve real control events span A/B/clear/C; each job lasts two complete
/// visits of a thread. All other fixture threads explicitly remain unattributed.
pub(super) fn transition(round: u64) -> Option<WorkBindingUpdate> {
    let cycle = round / CYCLE;
    let thread = (round / 8) % fixture::THREADS;
    if !round.is_multiple_of(8) || !THREADS.contains(&thread) {
        return None;
    }
    let job = match cycle {
        0 => Some("a"),
        2 => Some("b"),
        4 => None,
        6 => Some("c"),
        _ => return None,
    };
    Some(WorkBindingUpdate {
        version: 1,
        native_project_id: job.map(|_| PROJECT.into()),
        thread_id: fixture::thread_id(thread),
        workstream_id: job.map(|job| format!("benchmark-work-{job}")),
        outcome_id: job.map(|job| format!("benchmark-outcome-{job}")),
        transition_sequence: cycle / 2 + 1,
        source: "scale-fixture-agent".into(),
        authority: "explicit_agent_declaration".into(),
    })
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct WorkTotals {
    operations: u64,
    provider_tokens: u64,
    interval_ms: u64,
}

impl WorkTotals {
    fn add_rounds(&mut self, first: u64, end: u64) {
        let tokens = fixture::token_totals(first * 7, end * 7);
        self.operations += (end - first) * 4;
        self.interval_ms += (end - first) * (3 * 50 + 130);
        self.provider_tokens += u64::try_from(tokens.input_tokens + tokens.output_tokens)
            .expect("positive provider total oracle");
    }
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct WorkExpectations {
    pub global: BTreeMap<String, WorkTotals>,
    pub current_thread: BTreeMap<String, WorkTotals>,
    pub finite_repository: BTreeMap<String, WorkTotals>,
}

impl WorkExpectations {
    pub(super) fn calculate() -> Self {
        let mut expected = Self::default();
        let range = fixture::review_range();
        let finite_start = (range.start_ms() - fixture::START_MS) as u64 / 250;
        let finite_end = (range.end_ms() - fixture::START_MS) as u64 / 250;
        // Independent visit windows, rather than replaying transition() or
        // reading generated operation/token rows back as their own oracle.
        for (job, cycles) in [("a", 0..2), ("b", 2..4), ("c", 6..8)] {
            let outcome = format!("benchmark-outcome-{job}");
            for thread in THREADS {
                for cycle in cycles.clone() {
                    let first = cycle * CYCLE + thread * 8;
                    let end = (first + 8).min(fixture::ROUNDS);
                    if first >= end {
                        continue;
                    }
                    expected
                        .global
                        .entry(outcome.clone())
                        .or_default()
                        .add_rounds(first, end);
                    if thread == fixture::CURRENT_THREAD {
                        expected
                            .current_thread
                            .entry(outcome.clone())
                            .or_default()
                            .add_rounds(first, end);
                    }
                    let first = first.max(finite_start);
                    let end = end.min(finite_end);
                    if first < end {
                        expected
                            .finite_repository
                            .entry(outcome.clone())
                            .or_default()
                            .add_rounds(first, end);
                    }
                }
            }
        }
        expected
    }
}

/// Compare native user-visible job numbers with independently calculated values.
pub(super) fn check(
    report: &Value,
    wanted: &BTreeMap<String, WorkTotals>,
    total_operations: u64,
    scope: &str,
    failures: &mut Vec<String>,
) -> Value {
    let mut observed = BTreeMap::new();
    for row in report["outcomes"]["rows"].as_array().into_iter().flatten() {
        let key = serde_json::to_string(&json!([
            row["nativeProjectId"],
            row["workstreamId"],
            row["outcomeId"]
        ]))
        .expect("context key");
        let value = json!({"operations":row["effort"]["operations"],
            "providerTokens":row["effort"]["providerTotalTokens"]["measured"],
            "unknownTokens":row["effort"]["providerTotalTokens"]["unknown"],
            "elapsedMs":row["effort"]["elapsedExecutionMs"]["measured"],
            "unknownElapsedIntervals":row["effort"]["elapsedExecutionMs"]["unknown"]});
        if observed.insert(key, value).is_some() {
            failures.push(format!("{scope} repeats a job aggregate"));
        }
    }
    let mut expected = BTreeMap::new();
    let mut operations = 0;
    let mut tokens = 0;
    for (outcome, totals) in wanted {
        // Every bound fixture round has four operations: tools [0,50],
        // [30,80], [60,110], and model [10,140]. Their union is 140 ms;
        // the independent duration-sum oracle remains 280 ms per round.
        let rounds = totals.operations / 4;
        if totals.operations % 4 != 0 || totals.interval_ms != rounds * 280 {
            failures.push(format!(
                "{scope} work oracle does not contain complete fixture rounds"
            ));
        }
        let job = outcome
            .strip_prefix("benchmark-outcome-")
            .expect("oracle outcome");
        let key =
            serde_json::to_string(&json!([PROJECT, format!("benchmark-work-{job}"), outcome]))
                .expect("oracle context key");
        expected.insert(
            key,
            json!({"operations":totals.operations,"providerTokens":totals.provider_tokens,
            "unknownTokens":0,"elapsedMs":rounds * 140,"unknownElapsedIntervals":0}),
        );
        operations += totals.operations;
        tokens += totals.provider_tokens;
    }
    let baseline_operations = if scope == "global" {
        fixture::ROUNDS * 4
    } else {
        total_operations
    };
    let extra_operations = total_operations.saturating_sub(baseline_operations);
    if baseline_operations % 4 != 0 || total_operations < baseline_operations {
        failures.push(format!(
            "{scope} operation count is inconsistent with fixture rounds"
        ));
    }
    let diagnostic_interval_sum = baseline_operations / 4 * 280 + extra_operations * 20;
    let actual = json!({"rows":observed,"boundOperations":report["workBindings"]["boundOperations"],
        "unknownOperations":report["workBindings"]["unknownOperations"],
        "attributedOperations":report["outcomes"]["attributed"]["operations"],
        "unattributedOperations":report["outcomes"]["unattributed"]["operations"],
        "attributedTokens":report["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        "diagnosticIntervalSumMs":report["counts"]["interval_sum_ms"]});
    let expected = json!({"rows":expected,"boundOperations":operations,
        "unknownOperations":total_operations-operations,"attributedOperations":operations,
        "unattributedOperations":total_operations-operations,"attributedTokens":tokens,
        "diagnosticIntervalSumMs":diagnostic_interval_sum});
    if actual != expected {
        failures.push(format!("{scope} exact sparse job totals differ"));
    }
    json!({"actual":actual,"expected":expected,"matches":actual==expected})
}

/// Keep the existing finite public-review timer authoritative. These extra
/// public reports and scan diagnostics run afterward and retain their own time.
pub(super) fn measure(
    root: &Path,
    warm: &UsageReader,
    expected: &fixture::Expectations,
    finite_review: &Value,
    failures: &mut Vec<String>,
) -> Value {
    let Some(work) = &expected.work else {
        failures.push("retained fixture predates sparse assignments; generate a .6 fixture".into());
        return json!({"missingSparseOracle":true});
    };
    let finite = check(
        finite_review,
        &work.finite_repository,
        expected.finite_repository_counts.operations,
        "finite repository",
        failures,
    );
    let start = Instant::now();
    let current = UsageReader::open_at(root)
        .expect("current work reader")
        .performance_review(
            TimeRange::all(),
            &UsageFilter::default().for_thread(fixture::thread_id(fixture::CURRENT_THREAD)),
        )
        .expect("native current-chat attribution");
    let current_ms = start.elapsed().as_secs_f64() * 1_000.0;
    let current = check(
        &current,
        &work.current_thread,
        expected.current_thread_counts.operations,
        "current chat",
        failures,
    );
    let start = Instant::now();
    let completed = GlobalReviewReader::open_at(root)
        .expect("completed global review reader")
        .expect("background global review was prepared before foreground timing");
    assert_eq!(
        completed.snapshot().source_watermark,
        warm.manifest().watermark,
        "global attribution must use the pinned fixture source prefix"
    );
    assert_eq!(
        completed.snapshot().included_journal_watermark,
        warm.journal_watermark(),
        "the completed report must not mix a newer capture prefix"
    );
    let global = completed.report();
    let global_ms = start.elapsed().as_secs_f64() * 1_000.0;
    let global = check(
        global,
        &work.global,
        expected.global_counts.operations,
        "global",
        failures,
    );
    let scans = scan_evidence(root, warm, failures);
    json!({"finiteRepository":finite,"currentChat":current,"global":global,
        "additionalCurrentChatReviewMs":current_ms,"additionalGlobalReviewMs":global_ms,"scanEvidence":scans})
}

/// Retain the engine plan; report compact evidence that attribution uses the
/// selected slice and never adds a full canonical-history Parquet scan.
pub(super) fn scan_evidence(
    root: &Path,
    reader: &UsageReader,
    failures: &mut Vec<String>,
) -> Value {
    let filter = UsageFilter::default().for_repository(fixture::review_repository());
    let range = fixture::review_range();
    let query = crate::report_query::ReportQuery::new(reader, range, &filter)
        .expect("finite attribution query");
    let missing: u64 = query.connection.query_row(&format!(
        "SELECT count(DISTINCT c.operation_id) FROM ({}) c WHERE c.operation_id IS NOT NULL \
         AND ({}) AND NOT EXISTS (SELECT 1 FROM ({}) o WHERE o.operation_id=c.operation_id)",
        query.contributions,crate::review_aggregate::TOKEN_PREDICATE,query.operations
    ),[],|row| row.get(0)).expect("selected canonical request links");
    let lookups: u64 = query
        .connection
        .query_row("SELECT count(*) FROM work_operation_lookup", [], |row| {
            row.get(0)
        })
        .expect("bounded exact lookup evidence");
    let mut statement = query
        .connection
        .prepare(&format!("EXPLAIN {}", query.work_contributions()))
        .expect("explain selected attribution");
    let plan = statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("engine plan")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("complete engine plan")
        .join("\n");
    let extra_scan = plan.to_ascii_lowercase().contains("parquet");
    let projection = crate::review_projection::load_review_projection(root, reader.manifest())
        .expect("projection metadata")
        .expect("published operations");
    let files = crate::review_index::select_review_files(root, &projection, range, &filter)
        .expect("indexed finite files");
    let rounds = ((range.end_ms() - range.start_ms()) / 250) as u64;
    let file_bound = (rounds * 15).div_ceil(crate::review_state::REVIEW_PARTITION_SIZE) + 2;
    if missing != 0
        || lookups != 0
        || extra_scan
        || files.len() as u64 > file_bound
        || files.len() >= projection.partitions.len()
    {
        failures.push(
            "finite attribution lost canonical request links or scanned outside its indexed slice"
                .into(),
        );
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let file = format!("attribution-query-plan-{}-{stamp}.txt", std::process::id());
    let artifact = root
        .parent()
        .and_then(Path::parent)
        .expect("scale artifact");
    fs::write(artifact.join(&file), plan).expect("retain verbose engine evidence");
    json!({"unresolvedCanonicalReferences":missing,"extraOperationLookups":lookups,"extraParquetScan":extra_scan,
        "selectedOperationFiles":files.len(),"operationFileBound":file_bound,"fullHistoryOperationFiles":projection.partitions.len(),"planArtifact":file})
}
