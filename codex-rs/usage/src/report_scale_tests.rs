use super::*;
use pretty_assertions::assert_eq;
use std::time::Instant;

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires explicit disk-backed scale storage and measures process memory"]
async fn accounting_scale_bounds_memory_refresh_and_wal() {
    const REQUESTS: i64 = 20_000;
    let root = std::env::var_os("CXM_USAGE_SCALE_ROOT").expect("explicit scale storage");
    let temp = tempfile::tempdir_in(root).expect("isolated scale directory");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store.register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 0).await.expect("process");
    for index in 0..4 {
        insert_thread(&store, &format!("scale-{index}")).await;
        sqlx::query("INSERT INTO agents(id,thread_id,parent_agent_id,role_kind,created_at_ms) VALUES (?, ?, NULL, 'root', 0)")
            .bind(format!("agent-{index}")).bind(format!("scale-{index}"))
            .execute(&store.pool).await.expect("agent");
    }
    let mut repositories = Vec::new();
    for index in 0..8 {
        repositories.push(store.resolve_repository(&identity(&format!("/scale/{index}")), &label(&format!("scale-{index}")), /*observed_at_ms*/ 0).await.expect("repository"));
    }
    let mut connection = store.pool.acquire().await.expect("seed connection");
    sqlx::raw_sql("CREATE TEMP TABLE seed(n INTEGER PRIMARY KEY); CREATE TEMP TABLE seed_repositories(n INTEGER PRIMARY KEY, id TEXT);
        CREATE TEMP TABLE seed_components(n INTEGER PRIMARY KEY, category TEXT, tokens INTEGER);
        INSERT INTO seed_components VALUES (0,'input_tokens',1000),(1,'input_tokens_details.cached_tokens',500),(2,'input_tokens_details.cache_write_tokens',100),(3,'output_tokens',50),(4,'total_tokens',1050),(5,'output_tokens_details.reasoning_tokens',10);")
        .execute(&mut *connection).await.expect("seed tables");
    for (index, repository) in repositories.iter().enumerate() {
        sqlx::query("INSERT INTO seed_repositories VALUES (?, ?)").bind(index as i64).bind(repository.as_str()).execute(&mut *connection).await.expect("seed repository");
    }
    use sqlx::Connection;
    for start in (0..REQUESTS).step_by(256) {
        let end = (start + 256).min(REQUESTS);
        let mut tx = connection.begin().await.expect("bounded seed transaction");
        sqlx::query("DELETE FROM seed").execute(&mut *tx).await.expect("clear seed");
        sqlx::query("INSERT INTO seed WITH RECURSIVE sequence(n) AS (SELECT ?1 UNION ALL SELECT n+1 FROM sequence WHERE n+1 < ?2) SELECT n FROM sequence")
            .bind(start).bind(end).execute(&mut *tx).await.expect("seed page");
        sqlx::query("INSERT INTO operations(id,process_id,thread_id,agent_id,operation_kind,started_at_ms,taxonomy_version,phase,activity,activity_state,attribution_provenance)
            SELECT printf('10000000-0000-4000-8000-%012x',n),?,'scale-'||(n%4),'agent-'||(n%4),'model_request',n*10000,1,'implementation','coding','model_active','agent_declared' FROM seed")
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
            SELECT printf('60000000-0000-4000-8000-%012x',n),printf('10000000-0000-4000-8000-%012x',n),'completed',1,n*10000+1000,1000000000 FROM seed")
            .execute(&mut *tx).await.expect("terminals");
        sqlx::query("INSERT INTO coverage_events(event_id,operation_id,scope_kind,coverage_state,occurred_at_ms)
            SELECT printf('70000000-0000-4000-8000-%012x',seed.n*2+marker.n),printf('10000000-0000-4000-8000-%012x',seed.n),'model_attempt',CASE marker.n WHEN 0 THEN 'capture_started' ELSE 'partial' END,seed.n*10000+marker.n*1000
            FROM seed JOIN seed_components AS marker ON marker.n < 2")
            .execute(&mut *tx).await.expect("capture coverage");
        tx.commit().await.expect("commit bounded page");
    }
    drop(connection);
    let raw_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_observations").fetch_one(&store.pool).await.expect("raw count");
    assert_eq!(raw_before,REQUESTS*6);
    store.report_refresh.cancel();
    sqlx::query("DELETE FROM _usage_report_cache_meta").execute(&store.pool).await.expect("cold derived cache");
    crate::report_cache::prepare(&store.pool).await.expect("prepare historical frontier");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&store.pool).await.expect("isolated checkpoint");
    let mut reader = store.pool.begin().await.expect("external reader");
    sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM token_observations").fetch_one(&mut *reader).await.expect("pin snapshot");
    let mut pressure_seen = false;
    for _ in 0..5_000 {
        match crate::report_cache::backfill::step(&store.pool).await.expect("bounded refresh page") {
            crate::report_cache::backfill::Progress::Ready => break,
            crate::report_cache::backfill::Progress::Advanced => {},
            crate::report_cache::backfill::Progress::ReaderBusy => { pressure_seen = true; break; },
        }
    }
    assert!(pressure_seen,"a pinned reader must pause derived WAL growth");
    let (_,written,checkpointed): (i64,i64,i64) = sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)").fetch_one(&store.pool).await.expect("pressure evidence");
    // Canonical capture remains available while derived refresh is paused.
    let request = ModelRequestId::from_string("20000000-0000-4000-8000-000000000000").expect("request id");
    let mut late = token(request,RepositoryBucket::Single(repositories[0].clone()),Some(7),CoverageState::Complete);
    late.category_path = TokenCategoryPath::new("total_tokens").expect("provider total");
    store.record_token_observation(&late).await.expect("capture during pressure");
    reader.rollback().await.expect("release external snapshot");
    assert!(!matches!(crate::report_cache::backfill::step(&store.pool).await.expect("reader released"),crate::report_cache::backfill::Progress::ReaderBusy));
    let started = Instant::now();
    let cold = store.usage_summary(UsageSummaryScope::All).await.expect("bounded raw fallback");
    let cold_ms = started.elapsed().as_millis();
    let started = Instant::now();
    crate::report_cache::ensure(&store.pool,&store.report_refresh.reader).await.expect("complete refresh");
    let refresh_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let warm = store.usage_summary(UsageSummaryScope::All).await.expect("warm rollups");
    let warm_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let repeated = store.usage_summary(UsageSummaryScope::All).await.expect("repeated rollups");
    let repeated_ms = started.elapsed().as_millis();
    let measured_total = warm.tokens.iter().filter(|t|t.category_path == "total_tokens").map(|t|t.measured_tokens).sum::<i64>();
    let raw_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_observations").fetch_one(&store.pool).await.expect("preserved facts");
    let dimensional_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _usage_report_dimension_tokens").fetch_one(&store.pool).await.expect("compact dimensions");
    let status = std::fs::read_to_string("/proc/self/status").expect("Linux memory evidence");
    let peak_rss_kib: u64 = status.lines().find_map(|line|line.strip_prefix("VmHWM:")).expect("peak RSS").split_whitespace().next().expect("RSS value").parse().expect("RSS number");
    let checks = [
        ("cold and warm summaries agree",cold == warm),
        ("repeated summaries agree",warm == repeated),
        ("all operations counted",warm.operation_count == REQUESTS as u64),
        ("late total captured once",measured_total == REQUESTS*1050+7),
        ("raw history preserved",raw_after == raw_before+1),
        ("dimensions compressed",dimensional_rows < raw_after/10),
        ("128 MiB target",peak_rss_kib <= 128*1024),
    ];
    println!("{}",serde_json::json!({"fixture":"synthetic SQLite accounting history","requests":REQUESTS,"raw_observations":raw_after,"dimension_rows":dimensional_rows,"cold_raw_ms":cold_ms,"refresh_ms":refresh_ms,"warm_ms":warm_ms,"repeated_ms":repeated_ms,"peak_rss_kib":peak_rss_kib,"pinned_wal_pages":written-checkpointed,"provider_total_tokens":measured_total,"checks":checks}));
    assert!(checks.iter().all(|(_,passed)|*passed),"scale acceptance failed: {checks:?}");
}
