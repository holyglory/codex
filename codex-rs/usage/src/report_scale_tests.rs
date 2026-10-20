use super::*;
use crate::PerformanceReviewQuery;
use pretty_assertions::assert_eq;
use std::time::Instant;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn finite_review_shape_includes_root_descendants() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 0)
        .await
        .expect("process");
    sqlx::query("INSERT INTO threads(id,parent_thread_id,source_kind,created_at_ms) VALUES ('scale-root',NULL,'cli',0),('scale-child','scale-root','subagent',0)")
        .execute(&store.pool).await.expect("threads");
    let mut root = operation(process, "scale-root", OperationKind::ModelRequest);
    root.started_at_ms = 1_000;
    let root_request = record_request(&store, &root).await;
    let mut child = operation(process, "scale-child", OperationKind::ModelRequest);
    child.started_at_ms = 1_100;
    let child_request = record_request(&store, &child).await;
    for (operation, ended_at_ms) in [(&root, 2_000), (&child, 2_100)] {
        sqlx::query("INSERT INTO operation_events(event_id,operation_id,event_kind,terminal,occurred_at_ms,duration_ns) VALUES (?,?,'completed',1,?,1000000000)")
            .bind(FactEventId::new().as_string())
            .bind(operation.id.as_string())
            .bind(ended_at_ms)
            .execute(&store.pool).await.expect("terminal");
    }
    for (request, count) in [(root_request, 11), (child_request, 13)] {
        store
            .record_token_observation(&NewTokenObservation {
                id: FactEventId::new(),
                source_event_id: FactEventId::new(),
                source: TokenObservationSource::ModelRequest(request),
                category_path: TokenCategoryPath::new("total_tokens").expect("category"),
                token_count: Some(count),
                unit: TokenUnit::Tokens,
                measurement_provenance: MeasurementProvenance::ProviderReported,
                coverage_state: CoverageState::Complete,
                repository_bucket: RepositoryBucket::Unknown,
                observed_at_ms: 1_500,
            })
            .await
            .expect("token");
    }
    let root_query = |include_descendants| PerformanceReviewQuery {
        thread_id: Some(ThreadId::new("scale-root").expect("root")),
        include_descendants,
        time_range: Some(UtcTimeRange::new(/*start_ms*/ 900, /*end_ms*/ 2_200).expect("range")),
        ..Default::default()
    };
    let root_only = store
        .performance_review_packet(root_query(false))
        .await
        .expect("root review");
    let tree = store
        .performance_review_packet(root_query(true))
        .await
        .expect("tree review");
    assert_eq!(
        (
            root_only.coverage.raw_operations,
            root_only.coverage.raw_token_observations
        ),
        (1, 1)
    );
    assert_eq!(
        (
            tree.coverage.raw_operations,
            tree.coverage.raw_token_observations
        ),
        (2, 2)
    );
    assert_eq!(tree.tokens[0].measured_tokens, 24);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires explicit disk-backed scale storage and measures process memory"]
async fn accounting_scale_bounds_memory_refresh_and_wal() {
    let requests = std::env::var("CXM_USAGE_SCALE_REQUESTS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(20_000);
    assert!(requests >= 20_000);
    let retained_home = std::env::var_os("CXM_USAGE_SCALE_RETAIN").map(std::path::PathBuf::from);
    if let Some(path) = &retained_home {
        assert!(
            requests >= 1_300_000,
            "retention requires at least 1,300,000 requests"
        );
        assert!(!path.exists(), "retention output must be absent: {path:?}");
        assert!(
            path.parent().is_some_and(std::path::Path::exists),
            "retention parent must exist: {path:?}"
        );
    }
    let prefix = format!("{}.{}.{}_", "x".repeat(60), "y".repeat(60), "z".repeat(50));
    TokenCategoryPath::new(format!("{prefix}00000_tokens")).expect("valid long category");
    let root = std::env::var_os("CXM_USAGE_SCALE_ROOT").expect("explicit scale storage");
    let mut temp = tempfile::tempdir_in(root).expect("isolated scale directory");
    // Keep the complete synthetic working directory if a release-scale
    // assertion fails. It is diagnostic evidence, not a ready fixture.
    temp.disable_cleanup(retained_home.is_some());
    let store = UsageStore::open(temp.path()).await.expect("store");
    // The opener schedules its normal refresh worker. Stop it before seeding
    // the disposable history so fixture construction has one writer and cannot
    // fail spuriously with SQLITE_BUSY.
    store.report_refresh.cancel();
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 0)
        .await
        .expect("process");
    insert_thread(&store, "scale-root").await;
    sqlx::query("INSERT INTO agents(id,thread_id,parent_agent_id,role_kind,created_at_ms) VALUES ('agent-root', 'scale-root', NULL, 'root', 0)")
        .execute(&store.pool).await.expect("root agent");
    for index in 0..63 {
        let thread = format!("scale-{index:02}");
        sqlx::query("INSERT INTO threads(id,parent_thread_id,source_kind,created_at_ms) VALUES (?, 'scale-root', 'subagent', 0)")
            .bind(&thread).execute(&store.pool).await.expect("child thread");
        sqlx::query("INSERT INTO agents(id,thread_id,parent_agent_id,role_kind,created_at_ms) VALUES (?, ?, 'agent-root', 'worker', 0)")
            .bind(format!("agent-{index:02}")).bind(thread)
            .execute(&store.pool).await.expect("child agent");
    }
    let mut repositories = Vec::new();
    for index in 0..8 {
        repositories.push(
            store
                .resolve_repository(
                    &identity(&format!("/scale/{index}")),
                    &label(&format!("scale-{index}")),
                    /*observed_at_ms*/ 0,
                )
                .await
                .expect("repository"),
        );
    }
    let mut connection = store.pool.acquire().await.expect("seed connection");
    sqlx::raw_sql("CREATE TEMP TABLE seed(n INTEGER PRIMARY KEY); CREATE TEMP TABLE seed_repositories(n INTEGER PRIMARY KEY, id TEXT);
        CREATE TEMP TABLE seed_components(n INTEGER PRIMARY KEY, category TEXT, tokens INTEGER);
        INSERT INTO seed_components VALUES (0,'input_tokens',1000),(1,'input_tokens_details.cached_tokens',500),(2,'input_tokens_details.cache_write_tokens',100),(3,'output_tokens',50),(4,'total_tokens',1050),(5,'output_tokens_details.reasoning_tokens',10);")
        .execute(&mut *connection).await.expect("seed tables");
    for (index, repository) in repositories.iter().enumerate() {
        sqlx::query("INSERT INTO seed_repositories VALUES (?, ?)")
            .bind(index as i64)
            .bind(repository.as_str())
            .execute(&mut *connection)
            .await
            .expect("seed repository");
    }
    use sqlx::Connection;
    // Keep each fixture transaction large enough to exercise production-sized
    // history without spending most of the run in per-page transaction setup.
    // The refresh implementation still enforces its own bounded backfill
    // pages; this only controls disposable fixture construction.
    for start in (0..requests).step_by(8_192) {
        let end = (start + 8_192).min(requests);
        let mut tx = connection.begin().await.expect("bounded seed transaction");
        sqlx::query("DELETE FROM seed")
            .execute(&mut *tx)
            .await
            .expect("clear seed");
        sqlx::query("INSERT INTO seed WITH RECURSIVE sequence(n) AS (SELECT ?1 UNION ALL SELECT n+1 FROM sequence WHERE n+1 < ?2) SELECT n FROM sequence")
            .bind(start).bind(end).execute(&mut *tx).await.expect("seed page");
        sqlx::query("INSERT INTO operations(id,process_id,thread_id,agent_id,operation_kind,started_at_ms,taxonomy_version,phase,activity,activity_state,attribution_provenance)
            SELECT printf('10000000-0000-4000-8000-%012x',n),?,printf('scale-%02d',n%63),printf('agent-%02d',n%63),'model_request',n*10000,1,
              CASE n%4 WHEN 0 THEN 'implementation' WHEN 1 THEN 'planning' WHEN 2 THEN 'testing' ELSE 'reporting' END,
              CASE n%4 WHEN 0 THEN 'coding' WHEN 1 THEN 'repository_analysis' WHEN 2 THEN 'integration_testing' ELSE 'review_feedback' END,
              'model_active','agent_declared' FROM seed")
            .bind(process.as_string()).execute(&mut *tx).await.expect("operations");
        sqlx::query("INSERT INTO operation_work_contexts(operation_id,native_project_id,workstream_id,outcome_id,provenance)
            SELECT printf('10000000-0000-4000-8000-%012x',n),'scale-project','scale-workstream','outcome-'||(n%8),'runtime_observed' FROM seed")
            .execute(&mut *tx).await.expect("prospective contexts");
        sqlx::query("INSERT INTO model_requests(id,operation_id,provider_kind,model,transport_kind,attempt_number,client_origin)
            SELECT printf('20000000-0000-4000-8000-%012x',n),printf('10000000-0000-4000-8000-%012x',n),'openai','gpt-6-astra','sse',1,'test' FROM seed")
            .execute(&mut *tx).await.expect("requests");
        sqlx::query("INSERT INTO repository_attributions(event_id,operation_id,repository_id,attribution_kind,provenance,occurred_at_ms)
            SELECT printf('30000000-0000-4000-8000-%012x',seed.n),printf('10000000-0000-4000-8000-%012x',seed.n),repository.id,'primary','runtime_observed',seed.n*10000 FROM seed JOIN seed_repositories AS repository ON repository.n = seed.n%8")
            .execute(&mut *tx).await.expect("repository attribution");
        sqlx::query("INSERT INTO token_observations(id,model_request_id,source_event_id,category_path,token_count,unit,measurement_provenance,coverage_state,repository_bucket,observed_at_ms)
            SELECT printf('40000000-0000-4000-8000-%012x',seed.n*6+component.n),printf('20000000-0000-4000-8000-%012x',seed.n),printf('50000000-0000-4000-8000-%012x',seed.n),component.category,component.tokens,'tokens','provider_reported','complete',repository.id,seed.n*10000+500
            FROM seed CROSS JOIN seed_components AS component JOIN seed_repositories AS repository ON repository.n = seed.n%8")
            .execute(&mut *tx).await.expect("token facts and all live projections");
        sqlx::query("INSERT INTO operation_events(event_id,operation_id,event_kind,terminal,occurred_at_ms,duration_ns)
            SELECT printf('60000000-0000-4000-8000-%012x',n),printf('10000000-0000-4000-8000-%012x',n),'completed',1,n*10000+1000,1000000000 FROM seed WHERE n%511<>0")
            .execute(&mut *tx).await.expect("terminals");
        sqlx::query("INSERT INTO activity_spans(id,operation_id,activity_state,started_at_ms)
            SELECT printf('90000000-0000-4000-8000-%012x',n),printf('10000000-0000-4000-8000-%012x',n),
              CASE n%3 WHEN 0 THEN 'external_wait' WHEN 1 THEN 'user_wait' ELSE 'blocked_wait' END,n*10000+250
            FROM seed WHERE n%16=0")
            .execute(&mut *tx).await.expect("wait spans");
        sqlx::query("INSERT INTO activity_span_events(event_id,activity_span_id,event_kind,occurred_at_ms)
            SELECT printf('a0000000-0000-4000-8000-%012x',n),printf('90000000-0000-4000-8000-%012x',n),'ended',n*10000+750
            FROM seed WHERE n%16=0")
            .execute(&mut *tx).await.expect("wait span ends");
        sqlx::query("INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,occurred_at_ms)
            SELECT printf('70000000-0000-4000-8000-%012x',seed.n*2+marker.n),printf('10000000-0000-4000-8000-%012x',seed.n),'model_attempt',CASE marker.n WHEN 0 THEN 'capture_started' ELSE 'partial' END,seed.n*10000+marker.n*1000
            FROM seed JOIN seed_components AS marker ON marker.n < 2")
            .execute(&mut *tx).await.expect("capture coverage");
        tx.commit().await.expect("commit bounded page");
    }
    drop(connection);
    let raw_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_observations")
        .fetch_one(&store.pool)
        .await
        .expect("raw count");
    let memory = || {
        let status = std::fs::read_to_string("/proc/self/status").expect("Linux memory evidence");
        let value = |prefix: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .expect("RSS value")
        };
        (value("VmRSS:"), value("VmHWM:"))
    };
    let (seed_rss_kib, seed_peak_rss_kib) = memory();
    assert_eq!(raw_before, requests * 6);
    store.report_refresh.cancel();
    sqlx::query("DELETE FROM _usage_report_cache_meta")
        .execute(&store.pool)
        .await
        .expect("cold derived cache");
    crate::report_cache::prepare(&store.pool)
        .await
        .expect("prepare historical frontier");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&store.pool)
        .await
        .expect("isolated checkpoint");
    let mut reader = store.pool.begin().await.expect("external reader");
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM token_observations")
        .fetch_one(&mut *reader)
        .await
        .expect("pin snapshot");
    let mut pressure_seen = false;
    for _ in 0..5_000 {
        match crate::report_cache::backfill::step(&store.pool)
            .await
            .expect("bounded refresh page")
        {
            crate::report_cache::backfill::Progress::Ready => break,
            crate::report_cache::backfill::Progress::Advanced => {}
            crate::report_cache::backfill::Progress::Superseded => {
                panic!("isolated scale cache changed owner")
            }
            crate::report_cache::backfill::Progress::ReaderBusy => {
                pressure_seen = true;
                break;
            }
        }
    }
    assert!(
        pressure_seen,
        "a pinned reader must pause derived WAL growth"
    );
    let (_, written, checkpointed): (i64, i64, i64) =
        sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)")
            .fetch_one(&store.pool)
            .await
            .expect("pressure evidence");
    // Canonical capture remains available while derived refresh is paused.
    let request =
        ModelRequestId::from_string("20000000-0000-4000-8000-000000000000").expect("request id");
    let mut late = token(
        request,
        RepositoryBucket::Single(repositories[0].clone()),
        Some(7),
        CoverageState::Complete,
    );
    late.category_path = TokenCategoryPath::new("total_tokens").expect("provider total");
    store
        .record_token_observation(&late)
        .await
        .expect("capture during pressure");
    reader.rollback().await.expect("release external snapshot");
    assert!(!matches!(
        crate::report_cache::backfill::step(&store.pool)
            .await
            .expect("reader released"),
        crate::report_cache::backfill::Progress::ReaderBusy
    ));
    let started = Instant::now();
    // Full-history raw equivalence is an internal fixture oracle. Public
    // requests use the finite-window budget and warming contract below.
    let mut oracle = store.pool.acquire().await.expect("oracle connection");
    let all = UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
        thread_id: None,
        repository_id: None,
        account_profile_ref: None,
        time_range: None,
    };
    let cold_window = UsageSummaryQuery {
        time_range: Some(UtcTimeRange::new(/*start_ms*/ 0, /*end_ms*/ 86_400_000).expect("window")),
        ..all.clone()
    };
    let cold = store
        .read_usage_summary(cold_window.clone(), &mut oracle)
        .await
        .expect("bounded raw fallback");
    let cold_ms = started.elapsed().as_millis();
    drop(oracle);
    let (cold_rss_kib, cold_peak_rss_kib) = memory();
    let started = Instant::now();
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("complete refresh");
    let refresh_ms = started.elapsed().as_millis();
    let (refresh_rss_kib, refresh_peak_rss_kib) = memory();
    let started = Instant::now();
    let mut oracle = store.pool.acquire().await.expect("oracle connection");
    let warm_window = store
        .read_usage_summary(cold_window, &mut oracle)
        .await
        .expect("warm finite-window rollups");
    let warm_window_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let warm = store
        .read_usage_summary(all.clone(), &mut oracle)
        .await
        .expect("warm rollups");
    let warm_ms = started.elapsed().as_millis();
    let (warm_rss_kib, warm_peak_rss_kib) = memory();
    let started = Instant::now();
    let repeated = store
        .read_usage_summary(all, &mut oracle)
        .await
        .expect("repeated rollups");
    let repeated_ms = started.elapsed().as_millis();
    drop(oracle);
    let (repeated_rss_kib, repeated_peak_rss_kib) = memory();
    let measured_total = warm
        .tokens
        .iter()
        .filter(|t| t.category_path == "total_tokens")
        .map(|t| t.measured_tokens)
        .sum::<i64>();
    let raw_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_observations")
        .fetch_one(&store.pool)
        .await
        .expect("preserved facts");
    let dimensional_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _usage_report_dimension_tokens")
            .fetch_one(&store.pool)
            .await
            .expect("compact dimensions");
    let store = if let Some(path) = retained_home.as_ref() {
        const WINDOW_START_MS: i64 = 6_000_000_000;
        const WINDOW_END_MS: i64 = 6_086_400_000;
        let mut expected_operations = [0_i64; 8];
        let mut expected_provider_tokens = [0_i64; 8];
        for index in 0..requests {
            let started_at_ms = index * 10_000;
            let ended_at_ms = (index % 511 != 0).then_some(started_at_ms + 1_000);
            if started_at_ms < WINDOW_END_MS
                && ended_at_ms.is_none_or(|ended_at_ms| ended_at_ms > WINDOW_START_MS)
            {
                expected_operations[(index % 8) as usize] += 1;
            }
            let observed_at_ms = started_at_ms + 500;
            if (WINDOW_START_MS..WINDOW_END_MS).contains(&observed_at_ms) {
                expected_provider_tokens[(index % 8) as usize] += 1_050;
            }
        }
        assert!(expected_operations.iter().all(|operations| *operations > 0));
        let expected_all_provider_tokens = requests * 1_050 + 7;
        assert!(
            crate::report_cache::is_ready(&store.pool)
                .await
                .expect("retained cache status")
        );
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&store.pool)
            .await
            .expect("retained checkpoint");
        store.close().await;

        let source_usage = temp.path().join("usage");
        let destination_usage = path.join("usage");
        std::fs::create_dir(path).expect("retained home");
        std::fs::create_dir(&destination_usage).expect("retained usage directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("retained home permissions");
            std::fs::set_permissions(&destination_usage, std::fs::Permissions::from_mode(0o700))
                .expect("retained usage permissions");
        }
        for name in ["usage.sqlite3", "repository-hmac.key"] {
            let source = source_usage.join(name);
            std::fs::copy(&source, destination_usage.join(name)).expect("retain fixture file");
        }
        std::fs::write(
            path.join("fixture-marker"),
            "Disposable isolated accounting verification copy. Never a production collector.\n",
        )
        .expect("fixture marker");
        let scope = serde_json::json!({
            "window_start_ms": WINDOW_START_MS,
            "window_end_ms": WINDOW_END_MS,
            "fixture_shape_version": 2,
            "expected_open_operations": (requests + 510) / 511,
            "expected_wait_spans": (requests + 15) / 16,
            "expected_thread_count": 64,
            "expected_phase_activity_pairs": 4,
            "expected_token_facts": requests * 6 + 1,
            "root_thread_id": "scale-root",
            "include_descendants": true,
            "expected_root_window_operations": expected_operations.iter().sum::<i64>(),
            "expected_root_window_provider_tokens": expected_provider_tokens.iter().sum::<i64>(),
            "expected_all_operations": requests,
            "expected_all_provider_tokens": expected_all_provider_tokens,
            "repositories": repositories.iter().enumerate().map(|(index, repository)| {
                serde_json::json!({
                    "name": format!("synthetic-{index}"),
                    "repository_id": repository.as_str(),
                    "expected_operations": expected_operations[index],
                    "expected_provider_tokens": expected_provider_tokens[index],
                })
            }).collect::<Vec<_>>(),
        });
        std::fs::write(
            path.join("scope.json"),
            serde_json::to_vec_pretty(&scope).expect("scope JSON"),
        )
        .expect("scope file");
        UsageStore::open(temp.path())
            .await
            .expect("reopen after retention")
    } else {
        store
    };
    insert_thread(&store, "wide").await;
    let wide_op = operation(process, "wide", OperationKind::ModelRequest);
    let wide_request = record_request(&store, &wide_op).await;
    for start in (0_i64..16_385).step_by(256) {
        let end = (start + 256).min(16_385);
        sqlx::query("INSERT INTO token_observations(id,model_request_id,source_event_id,category_path,token_count,unit,measurement_provenance,coverage_state,repository_bucket,observed_at_ms)
            WITH RECURSIVE sequence(n) AS (SELECT ?2 UNION ALL SELECT n+1 FROM sequence WHERE n+1 < ?3)
            SELECT printf('80000000-0000-4000-8000-%012x',n),?1,printf('90000000-0000-4000-8000-%012x',n),?4||printf('%05d',n)||'_tokens',1,'tokens','provider_reported','complete','unknown',1000+n FROM sequence")
            .bind(wide_request.as_string()).bind(start).bind(end).bind(&prefix).execute(&store.pool).await.expect("wide fixture page");
    }
    let wide = store
        .usage_summary_in_range(
            UsageSummaryScope::Thread(ThreadId::new("wide").expect("thread")),
            Some(UtcTimeRange::new(/*start_ms*/ 1_000, /*end_ms*/ 17_384).expect("range")),
        )
        .await
        .expect("maximum supported groups");
    let wide_groups = wide.tokens.len();
    let (wide_rss_kib, wide_peak_rss_kib) = memory();
    drop(wide);
    let too_large = matches!(
        store
            .usage_summary(UsageSummaryScope::Thread(
                ThreadId::new("wide").expect("thread")
            ))
            .await,
        Err(UsageStoreError::ReportTooLarge)
    );
    let all_raw: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_observations")
        .fetch_one(&store.pool)
        .await
        .expect("wide facts preserved");
    let compact_cost_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _usage_report_cost_totals")
            .fetch_one(&store.pool)
            .await
            .expect("compact cost groups");
    let peak_rss_kib = [
        memory().1,
        seed_peak_rss_kib,
        cold_peak_rss_kib,
        refresh_peak_rss_kib,
        warm_peak_rss_kib,
        repeated_peak_rss_kib,
        wide_peak_rss_kib,
    ]
    .into_iter()
    .max()
    .expect("memory samples");
    let accounting_peak_rss_kib = peak_rss_kib.saturating_sub(seed_peak_rss_kib);
    let checks = [
        ("cold and warm summaries agree", cold == warm_window),
        ("repeated summaries agree", warm == repeated),
        (
            "all operations counted",
            warm.operation_count == requests as u64,
        ),
        (
            "late total captured once",
            measured_total == requests * 1050 + 7,
        ),
        ("raw history preserved", raw_after == raw_before + 1),
        ("dimensions compressed", dimensional_rows < raw_after / 10),
        ("costs compressed", compact_cost_rows < requests / 100),
        ("maximum-width response accepted", wide_groups == 16_384),
        ("oversized response rejected", too_large),
        ("wide facts preserved", all_raw == raw_after + 16_385),
        ("128 MiB target", accounting_peak_rss_kib <= 128 * 1024),
    ];
    println!(
        "{}",
        serde_json::json!({"fixture":"production-shaped SQLite accounting history","requests":requests,"raw_observations":raw_after,"dimension_rows":dimensional_rows,"cold_raw_ms":cold_ms,"refresh_ms":refresh_ms,"warm_window_ms":warm_window_ms,"warm_ms":warm_ms,"repeated_ms":repeated_ms,"peak_rss_kib":peak_rss_kib,"accounting_peak_rss_kib":accounting_peak_rss_kib,"memory_kib":{"seed":[seed_rss_kib,seed_peak_rss_kib],"cold":[cold_rss_kib,cold_peak_rss_kib],"refresh":[refresh_rss_kib,refresh_peak_rss_kib],"warm":[warm_rss_kib,warm_peak_rss_kib],"repeated":[repeated_rss_kib,repeated_peak_rss_kib],"wide":[wide_rss_kib,wide_peak_rss_kib]},"compact_cost_rows":compact_cost_rows,"wide_response_groups":wide_groups,"wide_observations":16_385,"pinned_wal_pages":written-checkpointed,"provider_total_tokens":measured_total,"checks":checks})
    );
    assert!(
        checks.iter().all(|(_, passed)| *passed),
        "scale acceptance failed: {checks:?}"
    );
    store.close().await;
    temp.disable_cleanup(false);
}
