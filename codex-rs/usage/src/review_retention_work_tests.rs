//! Real journal/publication/retention queries, with independently specified totals.
use super::*;
use crate::WorkBindingUpdate;
use crate::capture_work_binding_transition;
use crate::report_query::ReportQuery;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::tempdir;

fn bind(home: &Path, job: &str, sequence: u64) {
    capture_work_binding_transition(
        home,
        WorkBindingUpdate {
            version: 1,
            native_project_id: Some(format!("project-{job}")),
            thread_id: "active".into(),
            workstream_id: Some(format!("work-{job}")),
            outcome_id: Some(format!("outcome-{job}")),
            transition_sequence: sequence,
            source: "agent".into(),
            authority: "explicit_agent_declaration".into(),
        },
    )
    .expect("explicit switch");
}

fn capture(root: &Path, records: Vec<UsageRecord>) {
    UsageJournal::new(root.join("events.jsonl"))
        .append_batch(records.into_iter().map(JournalPayload::Record).collect())
        .expect("real journal captures");
    publish(root);
}

fn totals(reader: &UsageReader, filter: &UsageFilter) -> Vec<Value> {
    let query = ReportQuery::new(reader, TimeRange::all(), filter).expect("real report query");
    query_rows(&query.connection, &format!(
        "WITH effort AS (SELECT work_native_project_id,workstream_id,work_outcome_id, \
         operation_count operations,interval_sum_ms,0::HUGEINT input_tokens,0::HUGEINT provider_tokens \
         FROM ({}) WHERE kind <> 'wait' UNION ALL \
         SELECT work_native_project_id,workstream_id,work_outcome_id,0,0,input_tokens,provider_total_tokens \
         FROM ({}) WHERE ({})) \
         SELECT work_native_project_id project,workstream_id workstream,work_outcome_id outcome, \
         sum(operations)::UBIGINT operations,sum(interval_sum_ms)::UBIGINT interval_ms, \
         COALESCE(sum(input_tokens),0)::BIGINT input_tokens, \
         COALESCE(sum(provider_tokens),0)::UBIGINT provider_tokens FROM effort GROUP BY ALL \
         ORDER BY project NULLS FIRST,workstream,outcome",
        query.operation_statistics, query.work_contributions(), crate::review_aggregate::TOKEN_PREDICATE
    ), /*max_rows*/ 20).expect("bounded per-job totals")
}

fn expected(job: &str, operations: u64, input: i64, provider: u64) -> Value {
    json!({"project":format!("project-{job}"),"workstream":format!("work-{job}"),
        "outcome":format!("outcome-{job}"),"operations":operations,
        "interval_ms":operations*1_000,"input_tokens":input,"provider_tokens":provider})
}

fn maintain_at(root: &Path, at: i64) {
    let mut maintenance = Default::default();
    for _ in 0..6 {
        crate::retention::maintain(root, RetentionPolicy::default(), at, &mut maintenance)
            .expect("existing retention transaction");
    }
}

#[test]
fn job_totals_survive_hourly_lifetime_restart_and_resumed_work() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    bind(home.path(), "a", /*sequence*/ 1);
    let mut a = started("a", DAY, "active").with_account("account-a");
    a.response_id = Some("response-a".into());
    let mut a_token = token("a-token", DAY + 500, "active", /*input*/ 10)
        .with_operation("provider-a")
        .with_account("account-a");
    a_token.response_id = a.response_id.clone();
    capture(&root, vec![a.clone(), finished(&a), a_token]);
    bind(home.path(), "b", /*sequence*/ 2);
    let b = started("b", 2 * DAY, "active").with_account("account-a");
    capture(
        &root,
        vec![
            b.clone(),
            finished(&b),
            token("b-token", 2 * DAY + 500, "active", /*input*/ 20)
                .with_operation("b")
                .with_account("account-a"),
        ],
    );
    UsageJournal::new(root.join("events.jsonl"))
        .append_correction(UsageCorrection::new(
            "signed",
            "a-token",
            3 * DAY / 1_000,
            /*input_tokens_delta*/ -14,
            /*output_tokens_delta*/ 0,
            "provider adjustment",
        ))
        .expect("signed correction before retirement");
    publish(&root);
    let filter = UsageFilter::default()
        .for_account("account-a")
        .for_repository("repository-a");
    let oracle = vec![
        expected(
            "a", /*operations*/ 1, /*input*/ -4, /*provider*/ 11,
        ),
        expected(
            "b", /*operations*/ 1, /*input*/ 20, /*provider*/ 21,
        ),
    ];
    let pinned = UsageReader::open_at(&root).expect("pin detail");
    assert_eq!(totals(&pinned, &filter), oracle);
    maintain_at(&root, 4 * DAY);
    assert!(descriptor(&root).operation_rollup_files.is_empty());
    assert_eq!(
        totals(
            &UsageReader::open_at(&root).expect("under horizon"),
            &filter
        ),
        oracle
    );

    capture(
        &root,
        vec![
            UsageRecord::new_millis(
                "keep-active",
                11 * DAY,
                /*input_tokens*/ 0,
                /*output_tokens*/ 0,
            )
            .with_thread("active")
            .with_account("account-a")
            .with_repository("repository-a"),
        ],
    );
    maintain_at(&root, 12 * DAY);
    let hourly = descriptor(&root);
    assert_eq!(hourly.retired_operation_rows, 2);
    assert!(hourly.partitions.is_empty());
    let connection = crate::duckdb_connection::open_in_memory().expect("connection");
    let source = parquet_source(&root, &hourly.operation_rollup_files).expect("hourly rollups");
    let hours: u64 = connection
        .query_row(
            &format!("SELECT count(*) FROM {source} WHERE bucket_at_ms IS NOT NULL"),
            [],
            |row| row.get(0),
        )
        .expect("hour buckets");
    assert_eq!(hours, 2);
    assert_eq!(
        totals(
            &UsageReader::open_at(&root).expect("hourly restart"),
            &filter
        ),
        oracle
    );
    assert_eq!(totals(&pinned, &filter), oracle);

    maintain_at(&root, 20 * DAY);
    let lifetime = descriptor(&root);
    let source = parquet_source(&root, &lifetime.operation_rollup_files).expect("lifetime rollups");
    let lifetime_rows: u64 = connection
        .query_row(
            &format!("SELECT count(*) FROM {source} WHERE bucket_at_ms IS NULL"),
            [],
            |row| row.get(0),
        )
        .expect("lifetime buckets");
    assert_eq!(lifetime_rows, 2);
    assert_eq!(
        totals(
            &UsageReader::open_at(&root).expect("lifetime restart"),
            &filter
        ),
        oracle
    );
    UsageJournal::new(root.join("events.jsonl"))
        .append_correction(UsageCorrection::new(
            "expired-signed",
            "a-token",
            20 * DAY / 1_000,
            /*input_tokens_delta*/ -3,
            /*output_tokens_delta*/ 0,
            "after retirement",
        ))
        .expect("late correction");
    let pending = publish(&root);
    assert_eq!(
        pending.pending_corrections,
        vec!["expired-signed".to_string()]
    );
    // Once both target identity and detail have expired, a newly observed
    // correction first waits for its target. Aging its own observation makes
    // the existing publisher record the explicitly unapplied adjustment.
    maintain_at(&root, 28 * DAY);
    let expired = publish(&root);
    assert_eq!(expired.unapplied_retention_corrections, 1);
    let reader = UsageReader::open_at(&root).expect("truthful correction reader");
    assert_eq!(totals(&reader, &filter), oracle);
    assert_eq!(
        reader.summary(TimeRange::all(), &filter).expect("coverage")["coverage"]["unappliedCorrectionsAfterRetention"],
        json!(1)
    );

    bind(home.path(), "c", /*sequence*/ 3);
    let c = started("c", 29 * DAY, "active").with_account("account-a");
    capture(
        &root,
        vec![
            c.clone(),
            finished(&c),
            token("c-token", 29 * DAY + 500, "active", /*input*/ 30)
                .with_operation("c")
                .with_account("account-a"),
        ],
    );
    maintain_at(&root, 29 * DAY + 1_000);
    let resumed = UsageReader::open_at(&root).expect("resumed restart");
    let mut resumed_oracle = oracle;
    resumed_oracle.push(expected(
        "c", /*operations*/ 1, /*input*/ 30, /*provider*/ 31,
    ));
    assert_eq!(totals(&resumed, &filter), resumed_oracle);
    assert_eq!(
        resumed
            .summary(TimeRange::all(), &filter)
            .expect("ordinary totals")["inputTokens"],
        json!(46)
    );
    let query = ReportQuery::new(&resumed, TimeRange::all(), &filter).expect("retained source");
    let retained_count: u64 = query
        .connection
        .query_row(
            &format!(
                "SELECT sum(operation_count)::UBIGINT FROM ({})",
                query.retained_operation_statistics
            ),
            [],
            |row| row.get(0),
        )
        .expect("retained-only metrics");
    assert_eq!(retained_count, 2);
    assert!(totals(&resumed, &UsageFilter::default().for_account("other")).is_empty());
}

#[test]
fn ambiguous_corrupt_and_missing_links_are_not_guessed_at_retirement() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    bind(home.path(), "a", /*sequence*/ 1);
    let mut a = started("a", DAY, "active");
    a.response_id = Some("shared".into());
    capture(&root, vec![a.clone(), finished(&a)]);
    bind(home.path(), "b", /*sequence*/ 2);
    let mut b = started("b", 2 * DAY, "active");
    b.response_id = Some("shared".into());
    let mut unique = started("unique", 2 * DAY + 2_000, "active");
    unique.response_id = Some("only-b".into());
    let mut ambiguous = token("ambiguous", 2 * DAY, "active", /*input*/ 20);
    ambiguous.response_id = Some("shared".into());
    let mut conflict = token("conflict", 2 * DAY, "active", /*input*/ 30).with_operation("a");
    conflict.response_id = Some("only-b".into());
    let mut response = token("response", 2 * DAY, "active", /*input*/ 50);
    response.response_id = Some("only-b".into());
    capture(
        &root,
        vec![
            b.clone(),
            finished(&b),
            unique.clone(),
            finished(&unique),
            ambiguous,
            conflict,
            response,
            token("direct", DAY, "active", /*input*/ 10).with_operation("a"),
            token("missing", DAY, "active", /*input*/ 40).with_operation("absent"),
        ],
    );
    let oracle = vec![
        json!({"project":null,"workstream":null,"outcome":null,
        "operations":0,"interval_ms":0,"input_tokens":90,"provider_tokens":93}),
        expected(
            "a", /*operations*/ 1, /*input*/ 10, /*provider*/ 11,
        ),
        expected(
            "b", /*operations*/ 2, /*input*/ 50, /*provider*/ 51,
        ),
    ];
    let filter = UsageFilter::default();
    assert_eq!(
        totals(&UsageReader::open_at(&root).expect("live links"), &filter),
        oracle
    );
    std::fs::rename(
        root.join("work-bindings.jsonl"),
        root.join("work-bindings.saved"),
    )
    .expect("retain mapping before corruption injection");
    std::fs::write(root.join("work-bindings.jsonl"), b"invalid-mapping\n")
        .expect("corrupt only isolated mapping");
    let unmapped = started("unmapped", 3 * DAY, "active");
    capture(
        &root,
        vec![
            unmapped.clone(),
            finished(&unmapped),
            token("unmapped-token", 3 * DAY, "active", /*input*/ 1).with_operation("unmapped"),
        ],
    );
    let mut oracle = oracle;
    oracle[0]["operations"] = json!(1);
    oracle[0]["interval_ms"] = json!(1_000);
    oracle[0]["input_tokens"] = json!(91);
    oracle[0]["provider_tokens"] = json!(95);
    assert_eq!(
        totals(
            &UsageReader::open_at(&root).expect("corrupt mapping"),
            &filter
        ),
        oracle
    );
    maintain_at(&root, 12 * DAY);
    assert_eq!(
        totals(
            &UsageReader::open_at(&root).expect("retained links"),
            &filter
        ),
        oracle
    );
}

#[test]
fn old_v3_missing_columns_and_terminal_first_late_starts_remain_unattributed() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let first = started("legacy", DAY, "active");
    let late = started("late", DAY, "active");
    capture(
        &root,
        vec![first.clone(), finished(&first), finished(&late)],
    );
    capture(&root, vec![late]);
    let connection = crate::duckdb_connection::open_in_memory().expect("connection");
    let mut projection = descriptor(&root);
    let source =
        parquet_source(&root, &[projection.partitions[0].file.clone()]).expect("operations");
    let rows = query_rows::<ReviewOperation>(
        &connection,
        &format!("SELECT * FROM {source} ORDER BY operation_id"),
        /*max_rows*/ 10,
    )
    .expect("capture anchors");
    assert_eq!(
        rows.iter()
            .map(|row| (row.operation_id.as_str(), row.work_binding_start_sequence))
            .collect::<Vec<_>>(),
        vec![("late", None), ("legacy", Some(1))]
    );
    let old_file = "old-v3-operations.parquet";
    write_query(&connection, &root, old_file, &format!(
        "SELECT * EXCLUDE(work_native_project_id,workstream_id,work_outcome_id,work_binding_start_sequence) FROM {source}"))
        .expect("legacy operation schema");
    projection.partitions[0].file = old_file.into();
    let files = projection
        .contribution_fragments
        .iter()
        .map(|f| f.file.clone())
        .collect::<Vec<_>>();
    let source = parquet_source(&root, &files).expect("contributions");
    let old_file = "old-v3-contributions.parquet";
    write_query(
        &connection,
        &root,
        old_file,
        &format!(
            "SELECT * EXCLUDE(work_native_project_id,workstream_id,work_outcome_id) FROM {source}"
        ),
    )
    .expect("legacy contribution schema");
    projection.contribution_fragments.truncate(1);
    projection.contribution_fragments[0].file = old_file.into();
    let store = ManifestStore::new(root.join("manifest.json"));
    let mut manifest = store.load().expect("manifest");
    write_json(&root, "old-v3-projection.json", &projection).expect("legacy descriptor");
    manifest.review_projection_file = Some("old-v3-projection.json".into());
    store.save(&manifest).expect("publish legacy fixture");
    let reader = UsageReader::open_at(&root).expect("legacy restart");
    assert_eq!(
        totals(&reader, &UsageFilter::default()),
        vec![json!({"project":null,
        "workstream":null,"outcome":null,"operations":2,"interval_ms":2_000,
        "input_tokens":0,"provider_tokens":0})]
    );
    let query = ReportQuery::new(&reader, TimeRange::all(), &UsageFilter::default())
        .expect("nullable fallback");
    let anchors: u64 = query
        .connection
        .query_row(
            &format!(
                "SELECT count(work_binding_start_sequence) FROM ({})",
                query.operations
            ),
            [],
            |row| row.get(0),
        )
        .expect("no fabricated legacy anchor");
    assert_eq!(anchors, 0);
}

#[test]
fn late_token_uses_exact_operation_identity_outside_the_time_slice() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    capture_work_binding_transition(
        home.path(),
        WorkBindingUpdate {
            version: 1,
            native_project_id: Some("project-only".into()),
            thread_id: "active".into(),
            workstream_id: None,
            outcome_id: None,
            transition_sequence: 1,
            source: "agent".into(),
            authority: "explicit_agent_declaration".into(),
        },
    )
    .expect("project-only assignment");
    let mut start = started("early", DAY, "active");
    start.response_id = Some("early-response".into());
    capture(&root, vec![start.clone(), finished(&start)]);
    bind(home.path(), "b", /*sequence*/ 2);
    let mut legacy = token(
        "legacy-response",
        3 * DAY + 2_000,
        "active",
        /*input*/ 2,
    )
    .with_operation("early-response");
    legacy.response_id = Some("early-response".into());
    capture(
        &root,
        vec![
            token("late-direct", 3 * DAY, "active", /*input*/ 7).with_operation("early"),
            legacy,
        ],
    );
    let reader = UsageReader::open_at(&root).expect("late capture reader");
    let range = TimeRange::from_millis(3 * DAY, 3 * DAY + 1_000).expect("late token window");
    let query = ReportQuery::new(&reader, range, &UsageFilter::default()).expect("finite query");
    let operation_count: u64 = query
        .connection
        .query_row(
            &format!("SELECT count(*) FROM ({})", query.operations),
            [],
            |row| row.get(0),
        )
        .expect("time slice");
    assert_eq!(operation_count, 0);
    let work = query.work_contributions();
    let rows = query_rows::<Value>(&query.connection, &format!(
        "SELECT record_id,work_operation_id,work_native_project_id,workstream_id,work_outcome_id \
         FROM ({work}) ORDER BY record_id"), /*max_rows*/ 10).expect("indexed late identity");
    assert_eq!(
        rows,
        vec![
            json!({"record_id":"late-direct","work_operation_id":"early","work_native_project_id":"project-only",
            "workstream_id":null,"work_outcome_id":null})
        ]
    );
    let range = TimeRange::from_millis(3 * DAY + 2_000, 3 * DAY + 3_000).expect("legacy window");
    let query =
        ReportQuery::new(&reader, range, &UsageFilter::default()).expect("legacy finite query");
    let work = query.work_contributions();
    let rows = query_rows::<Value>(&query.connection, &format!(
        "SELECT record_id,work_operation_id,work_native_project_id,workstream_id,work_outcome_id \
         FROM ({work}) ORDER BY record_id"), /*max_rows*/ 10).expect("no global response scan");
    assert_eq!(
        rows,
        vec![
            json!({"record_id":"legacy-response","work_operation_id":null,
        "work_native_project_id":null,"workstream_id":null,"work_outcome_id":null})
        ]
    );

    // A stored tuple is authoritative as a whole. A nullable work ID must not
    // be filled from a different captured tuple on the linked operation.
    let query =
        ReportQuery::new(&reader, TimeRange::all(), &UsageFilter::default()).expect("all work");
    let operations = format!(
        "SELECT * REPLACE('other-project' AS work_native_project_id, \
        'other-stream' AS workstream_id,'other-outcome' AS work_outcome_id) FROM ({})",
        query.operations
    );
    let captured = format!(
        "SELECT * REPLACE('project-only' AS work_native_project_id, \
        NULL::VARCHAR AS workstream_id,NULL::VARCHAR AS work_outcome_id) FROM ({}) \
        WHERE record_id='late-direct'",
        query.contributions
    );
    let source = crate::review_aggregate::work_contribution_source(&captured, &operations);
    let tuple = query_rows::<Value>(
        &query.connection,
        &format!("SELECT work_native_project_id,workstream_id,work_outcome_id FROM ({source})"),
        /*max_rows*/ 1,
    )
    .expect("stored tuple precedence");
    assert_eq!(
        tuple,
        vec![
            json!({"work_native_project_id":"project-only","workstream_id":null,"work_outcome_id":null})
        ]
    );
}
