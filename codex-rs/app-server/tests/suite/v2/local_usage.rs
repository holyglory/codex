use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server::INVALID_PARAMS_ERROR_CODE;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::LocalUsageRepositoryIdentityParams;
use codex_app_server_protocol::LocalUsageRepositoryIdentityResponse;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::LocalUsageWorkContextBinding;
use codex_app_server_protocol::RequestId;
use codex_usage::JournalReadLimits;
use codex_usage::UsageCorrection;
use codex_usage::UsageJournal;
use codex_usage::UsageOutcome;
use codex_usage::UsagePublisher;
use codex_usage::UsageRecord;
use codex_usage::UsageStore;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

fn fixture_repository() -> Result<TempDir> {
    let directory = TempDir::new()?;
    let output = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(directory.path())
        .output()?;
    anyhow::ensure!(output.status.success(), "fixture Git initialization failed");
    Ok(directory)
}

fn fixture_fact(
    id: &str,
    thread: &str,
    turn: &str,
    repository: &str,
    base: i64,
    tokens: u64,
) -> UsageRecord {
    let mut fact = UsageRecord::new_millis(format!("{id}-finished"), base + 2_000, tokens, 0)
        .with_operation(id)
        .with_repository(repository)
        .with_thread(thread)
        .with_account("fixture-account")
        .with_activity("coding", "implementation");
    fact.turn_id = Some(turn.into());
    fact.event = Some("model_request_finished".into());
    fact.started_at_ms = Some(base + 1_000);
    fact.ended_at_ms = Some(base + 2_000);
    fact.duration_ms = Some(1_000);
    fact.outcome = Some(UsageOutcome::Completed);
    fact.coverage = Some("recorded".into());
    fact.provider_total_tokens = Some(tokens);
    fact
}

fn copy_fixture_tree(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "work-contexts.jsonl" || name.to_string_lossy().ends_with(".lock") {
            continue;
        }
        let target = destination.join(name);
        if entry.file_type()?.is_dir() {
            copy_fixture_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn export_runtime_fixture(home: &Path, info: &Value) -> Result<Option<PathBuf>> {
    let Some(root) = std::env::var_os("CODEX_USAGE_API_FIXTURE_DIR").map(PathBuf::from) else {
        return Ok(None);
    };
    if root.exists() && std::fs::read_dir(&root)?.next().is_some() {
        let prior: Value = serde_json::from_slice(&std::fs::read(root.join("fixture-info.json"))?)?;
        anyhow::ensure!(
            prior["fixtureKind"] == "codex-usage-v2-synthetic-v1",
            "existing fixture destination is not owned by this exporter"
        );
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        std::fs::rename(
            &root,
            root.with_file_name(format!("usage-v2-runtime-fixture-previous-{stamp}")),
        )?;
    }
    copy_fixture_tree(&home.join("usage-v2"), &root.join("usage-v2"))?;
    std::fs::write(
        root.join("fixture-info.json"),
        serde_json::to_vec_pretty(info)?,
    )?;
    anyhow::ensure!(
        !root.join("usage-v2/work-contexts.jsonl").exists(),
        "runtime fixture must not depend on a compatibility sidecar"
    );
    Ok(Some(root))
}

#[tokio::test]
async fn local_usage_request_bindings_pin_tail_across_scopes_and_measure_real_ipc() -> Result<()> {
    let home = TempDir::new()?;
    let repository_directory = fixture_repository()?;
    let repository_path = std::env::var_os("CODEX_USAGE_V2_FIXTURE_REPOSITORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| repository_directory.path().to_owned());
    let repository_path = repository_path.canonicalize()?;
    let repository = codex_core::usage_repository_id(&repository_path)
        .context("fixture repository is not recognized")?;
    let base = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )? - 10_000;
    let mut store = UsageStore::open(home.path())?;
    for (id, thread, turn, total) in [
        ("fixture-model", "fixture-thread", "fixture-turn", 120),
        (
            "fixture-other",
            "fixture-other-thread",
            "fixture-other-turn",
            40,
        ),
        (
            "fixture-unbound",
            "fixture-unbound-thread",
            "fixture-unbound-turn",
            15,
        ),
        (
            "fixture-unknown-stream",
            "fixture-unknown-thread",
            "fixture-unknown-turn",
            5,
        ),
    ] {
        store.append_record(fixture_fact(id, thread, turn, &repository, base, total))?;
    }
    UsagePublisher::open_at(store.root())?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    drop(store);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized_with_timeout(Duration::from_secs(20))
        .await?;
    let identity: LocalUsageRepositoryIdentityResponse = server
        .request(|request_id| ClientRequest::LocalUsageRepositoryIdentity {
            request_id,
            params: LocalUsageRepositoryIdentityParams {
                contract_version: Some(1),
                path: repository_path.to_string_lossy().into_owned(),
            },
        })
        .await?;
    assert_eq!(identity.repository_id.as_deref(), Some(repository.as_str()));
    assert_eq!(identity.format_version, 3);
    assert!(!serde_json::to_string(&identity)?.contains(repository_path.to_str().unwrap()));
    let publication_guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.path().join("usage-v2/.snapshot.lock"))?;
    publication_guard.lock()?;
    UsageJournal::new(home.path().join("usage-v2/events.jsonl")).append_record(fixture_fact(
        "fixture-tail",
        "fixture-tail-thread",
        "fixture-tail-turn",
        &repository,
        base + 500,
        7,
    ))?;
    let info = json!({"fixtureKind":"codex-usage-v2-synthetic-v1","repositoryPath":repository_path,"repositoryId":repository,
        "windowStartMs":base,"windowEndMs":base+5_000,"capturedAtMs":base+500,
        "operationId":"fixture-model","threadId":"fixture-thread","turnId":"fixture-turn","nativeProjectId":"fixture-project",
        "workstreamId":"fixture-workstream","outcomeId":"fixture-outcome","experimentRef":"fixture-experiment@1",
        "additionalOperations":[{"operationId":"fixture-other","threadId":"fixture-other-thread","turnId":"fixture-other-turn","providerTokens":40},
            {"operationId":"fixture-unbound","providerTokens":15},{"operationId":"fixture-unknown-stream","providerTokens":5},{"operationId":"fixture-tail","providerTokens":7}],
        "providerTokens":187});
    let exported = export_runtime_fixture(home.path(), &info)?;
    let global_start = Instant::now();
    let global: Value = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: LocalUsageSummaryParams::default(),
        })
        .await?;
    assert_eq!(
        ["review", "observations", "operations"].map(|field| global.get(field)),
        [Some(&Value::Null); 3]
    );
    let global: LocalUsageSummaryResponse = serde_json::from_value(global)?;
    let metadata_cold_ms = global_start.elapsed().as_millis();
    assert!(
        global.report["providerTokens"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        global.report["coverage"]["providerTotals"],
        "unavailable_unbounded"
    );
    assert!(
        global.snapshot.journal_watermark > global.snapshot.watermark,
        "test must pin an actual unpublished tail"
    );
    let declaration = |id: &str,
                       op: &str,
                       thread: &str,
                       turn: &str,
                       stream: Option<&str>,
                       outcome: &str|
     -> Result<LocalUsageWorkContextBinding> {
        Ok(serde_json::from_value(
            json!({"schemaVersion":1,"operationId":op,"capturedAtMs":base+500,"workContext":{
            "schemaVersion":1,"id":id,"repositoryId":"fixture-coordinator-repo","nativeProjectId":"fixture-project",
            "workstreamId":stream,"outcomeId":outcome,"threadId":thread,"turnId":turn,"experimentRef":"fixture-experiment@1"}}),
        )?)
    };
    let declarations = vec![
        declaration(
            "C1",
            "fixture-model",
            "fixture-thread",
            "fixture-turn",
            Some("fixture-workstream"),
            "fixture-outcome",
        )?,
        declaration(
            "C2",
            "fixture-other",
            "fixture-other-thread",
            "fixture-other-turn",
            Some("other-stream"),
            "other-outcome",
        )?,
        declaration(
            "C3",
            "fixture-unknown-stream",
            "fixture-unknown-thread",
            "fixture-unknown-turn",
            None,
            "unknown-stream-outcome",
        )?,
    ];
    let params = LocalUsageSummaryParams {
        from_at_ms: Some(base),
        to_at_ms: Some(base + 5_000),
        repository_id: Some(repository.clone()),
        include_review: true,
        workstream_id: Some("fixture-workstream".into()),
        work_context_bindings: Some(declarations.clone()),
        expected_snapshot: Some(global.snapshot.source_watermark.clone()),
        ..Default::default()
    };
    let cold_start = Instant::now();
    let scoped: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: params.clone(),
        })
        .await?;
    let finite_cold_ms = cold_start.elapsed().as_millis();
    assert_eq!(
        scoped.snapshot.source_watermark,
        global.snapshot.source_watermark
    );
    assert_eq!(scoped.report["providerTokens"][0]["measuredTokens"], 187);
    assert_ne!(
        scoped.work_contexts[0].repository_id.as_deref(),
        Some(repository.as_str())
    );
    let review = scoped.review.as_ref().unwrap();
    assert_eq!(review["totals"]["total_tokens"], 187);
    assert_eq!(
        review["outcomes"]["totals"]["providerTotalTokens"]["measured"],
        147
    );
    assert_eq!(
        review["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        120
    );
    assert_eq!(
        review["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
        27
    );
    assert_eq!(review["outcomes"]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(
        review["outcomes"]["rows"][0]["outcomeId"],
        "fixture-outcome"
    );
    for effort in [
        &review["outcomes"]["totals"],
        &review["outcomes"]["attributed"],
        &review["outcomes"]["unattributed"],
        &review["outcomes"]["rows"][0]["effort"],
    ] {
        assert_eq!(effort["elapsedExecutionMs"]["measured"], 0);
        assert_eq!(effort["elapsedExecutionMs"]["exact"], Value::Null);
    }
    assert!(
        scoped.report["measuredExecutionIntervalSumMs"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        scoped.work_contexts[0].native_project_id.as_deref(),
        Some("fixture-project")
    );
    assert_eq!(
        scoped.work_contexts[0].thread_id.as_deref(),
        Some("fixture-thread")
    );
    assert_eq!(
        scoped.work_contexts[0].experiment_ref.as_deref(),
        Some("fixture-experiment@1")
    );
    let mut warm_max_ms = 0;
    for _ in 0..3 {
        let warm_start = Instant::now();
        let warm: LocalUsageSummaryResponse = server
            .request(|request_id| ClientRequest::LocalUsageSummary {
                request_id,
                params: params.clone(),
            })
            .await?;
        let ms = warm_start.elapsed().as_millis();
        warm_max_ms = warm_max_ms.max(ms);
        assert!(ms < 500, "warm isolated RPC exceeded 500ms: {ms}");
        assert_eq!(warm.review, scoped.review);
    }
    let latency = json!({"metadataColdMs":metadata_cold_ms,"finiteReviewColdMs":finite_cold_ms,"warmMaximumMs":warm_max_ms,"warmSamples":3});
    eprintln!("USAGE_V2_LATENCY {latency}");
    if let Some(exported) = &exported {
        std::fs::write(
            exported.join("latency.json"),
            serde_json::to_vec_pretty(&latency)?,
        )?;
        std::fs::write(
            exported.join("bound-summary.json"),
            serde_json::to_vec_pretty(&scoped)?,
        )?;
    }
    UsageJournal::new(home.path().join("usage-v2/events.jsonl")).append_record(fixture_fact(
        "fixture-late",
        "fixture-late-thread",
        "fixture-late-turn",
        &repository,
        base + 1_500,
        11,
    ))?;
    for (account, thread, total) in [
        (Some("fixture-account"), None, 187),
        (None, Some("fixture-thread"), 120),
        (None, Some("fixture-tail-thread"), 7),
    ] {
        let response: LocalUsageSummaryResponse = server
            .request(|request_id| ClientRequest::LocalUsageSummary {
                request_id,
                params: LocalUsageSummaryParams {
                    account_id: account.map(str::to_owned),
                    thread_id: thread.map(str::to_owned),
                    expected_snapshot: Some(global.snapshot.source_watermark.clone()),
                    ..Default::default()
                },
            })
            .await?;
        assert_eq!(response.summary["inputTokens"], total);
        assert_eq!(
            response.snapshot.journal_watermark,
            global.snapshot.journal_watermark
        );
        assert_eq!(
            response.snapshot.source_watermark,
            global.snapshot.source_watermark
        );
    }
    let fresh: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: LocalUsageSummaryParams::default(),
        })
        .await?;
    assert_eq!(fresh.summary["inputTokens"], 198);
    assert!(fresh.snapshot.journal_watermark > global.snapshot.journal_watermark);
    let mut unscoped_params = params.clone();
    unscoped_params.expected_snapshot = Some(fresh.snapshot.source_watermark.clone());
    unscoped_params.workstream_id = None;
    let all: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: unscoped_params,
        })
        .await?;
    assert_eq!(
        all.review.as_ref().unwrap()["outcomes"]["totals"]["providerTotalTokens"]["measured"],
        198
    );
    assert_eq!(
        all.review.as_ref().unwrap()["outcomes"]["rows"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    drop(publication_guard);
    let mut invalids = Vec::new();
    let mut bad = declarations.clone();
    bad[0].work_context.thread_id = Some("wrong-thread".into());
    invalids.push((Some(repository.clone()), bad));
    let mut bad = declarations.clone();
    bad[0].work_context.turn_id = Some("wrong-turn".into());
    invalids.push((Some(repository.clone()), bad));
    let mut bad = declarations.clone();
    bad[0].work_context.experiment_ref = Some("invalid ref".into());
    invalids.push((Some(repository.clone()), bad));
    invalids.push((Some("unrelated-producer-repo".into()), declarations.clone()));
    let mut bad = declarations.clone();
    let mut conflict = bad[0].clone();
    conflict.work_context.id = "C9".into();
    bad.push(conflict);
    invalids.push((Some(repository.clone()), bad));
    invalids.push((Some(repository.clone()), vec![declarations[0].clone(); 257]));
    for (repository_id, work_context_bindings) in invalids {
        let mut invalid = params.clone();
        invalid.repository_id = repository_id;
        invalid.work_context_bindings = Some(work_context_bindings);
        invalid.expected_snapshot = None;
        let id = server
            .send_request("localUsage/summary", Some(serde_json::to_value(invalid)?))
            .await?;
        let error = timeout(
            Duration::from_secs(20),
            server.read_stream_until_error_message(RequestId::Integer(id)),
        )
        .await??;
        assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
    }
    let mut maximum = params.clone();
    maximum.expected_snapshot = None;
    maximum.work_context_bindings = Some(
        (0..256)
            .map(|index| {
                let mut line = declarations[0].clone();
                line.operation_id = format!("unobserved-{index}");
                line.work_context.id = format!("C{index}");
                line
            })
            .collect(),
    );
    let maximum: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: maximum,
        })
        .await?;
    assert!(maximum.operation_bindings.is_empty());
    // A pending correction cannot send an unbounded read down the historical
    // fallback. It reports source availability, while finite queries still work.
    let correction_guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.path().join("usage-v2/.snapshot.lock"))?;
    correction_guard.lock()?;
    UsageJournal::new(home.path().join("usage-v2/events.jsonl")).append_correction(
        UsageCorrection::new(
            "fixture-correction",
            "fixture-model-finished",
            (base + 4_000).div_euclid(1_000),
            1,
            0,
            "fixture correction",
        ),
    )?;
    let id = server
        .send_request("localUsage/summary", Some(json!({})))
        .await?;
    let unavailable = timeout(
        Duration::from_secs(20),
        server.read_stream_until_error_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(unavailable.error.code, -32603);
    assert_eq!(
        unavailable.error.data.as_ref().unwrap()["coverage"],
        "unavailable"
    );
    assert_eq!(
        unavailable.error.data.as_ref().unwrap()["reason"],
        "metadata_corrections_pending"
    );
    let pinned: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: LocalUsageSummaryParams {
                expected_snapshot: Some(maximum.snapshot.source_watermark.clone()),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(pinned.summary["inputTokens"], 198);
    let mut finite = params.clone();
    finite.expected_snapshot = None;
    let finite: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: finite,
        })
        .await?;
    assert_eq!(finite.summary["inputTokens"], 199);
    drop(correction_guard);
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    let id = server
        .send_request(
            "localUsage/summary",
            Some(json!({"expectedSnapshot":maximum.snapshot.source_watermark})),
        )
        .await?;
    let error = timeout(
        Duration::from_secs(20),
        server.read_stream_until_error_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
    Ok(())
}

#[tokio::test]
async fn local_usage_repository_identity_is_read_only_and_never_echoes_paths() -> Result<()> {
    let home = TempDir::new()?;
    let outside = TempDir::new()?;
    let repository = fixture_repository()?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized_with_timeout(Duration::from_secs(20))
        .await?;
    for path in [repository.path(), outside.path()] {
        let response: LocalUsageRepositoryIdentityResponse = server
            .request(|request_id| ClientRequest::LocalUsageRepositoryIdentity {
                request_id,
                params: LocalUsageRepositoryIdentityParams {
                    path: path.to_string_lossy().into_owned(),
                    ..Default::default()
                },
            })
            .await?;
        assert_eq!(
            response.repository_id,
            codex_core::usage_repository_id(path)
        );
        assert_eq!(response.format_version, 3);
        assert!(!serde_json::to_string(&response)?.contains(path.to_str().unwrap()));
    }
    let id = server
        .send_request(
            "localUsage/repositoryIdentity",
            Some(json!({"path":"private-relative-path"})),
        )
        .await?;
    let error = timeout(
        Duration::from_secs(20),
        server.read_stream_until_error_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
    assert!(!serde_json::to_string(&error)?.contains("private-relative-path"));
    Ok(())
}

#[tokio::test]
async fn local_usage_summary_reads_published_facts_and_preserves_unattributed_effort() -> Result<()>
{
    let home = TempDir::new()?;
    let legacy = home.path().join("usage.sqlite3");
    std::fs::write(&legacy, b"legacy usage must never be opened")?;
    let mut store = UsageStore::open(home.path())?;
    let mut start = UsageRecord::new_millis("started", 1_000, 0, 0)
        .with_operation("model-operation")
        .with_repository("repo-a")
        .with_account("account-a")
        .with_thread("thread-a")
        .with_activity("coding", "implementation");
    start.event = Some("model_request_started".into());
    start.started_at_ms = Some(1_000);
    start.classification = Some("agent_declared".into());
    let mut finish = start.clone();
    finish.id = "finished".into();
    finish.observed_at = 2;
    finish.observed_at_ms = Some(2_000);
    finish.event = Some("model_request_finished".into());
    finish.ended_at_ms = Some(2_000);
    finish.duration_ms = Some(1_000);
    finish.outcome = Some(UsageOutcome::Completed);
    finish.coverage = Some("recorded".into());
    let mut tokens = UsageRecord::new_millis("tokens", 2_000, 100, 20)
        .with_operation("model-operation")
        .with_repository("repo-a")
        .with_account("account-a")
        .with_thread("thread-a");
    tokens.provider_total_tokens = Some(120);
    tokens.cached_input_tokens = 80;
    tokens.reasoning_output_tokens = 5;
    tokens.event = Some("token_observation".into());
    tokens.coverage = Some("recorded".into());
    for record in [start, finish, tokens] {
        store.append_record(record)?;
    }
    let mut incomplete_tokens = UsageRecord::new_millis("no-provider-total", 2_000, 9, 1)
        .with_repository("repo-b")
        .with_account("account-b")
        .with_thread("thread-b");
    incomplete_tokens.event = Some("token_observation".into());
    incomplete_tokens.coverage = Some("recorded".into());
    store.append_record(incomplete_tokens)?;
    UsagePublisher::open_at(store.root())?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    drop(store);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized_with_timeout(Duration::from_secs(20))
        .await?;
    let params = LocalUsageSummaryParams {
        contract_version: Some(1),
        from_at_ms: Some(0),
        to_at_ms: Some(3_000),
        repository_id: Some("repo-a".into()),
        include_review: true,
        record_limit: 1,
        operation_limit: 1,
        ..Default::default()
    };
    let result: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: params.clone(),
        })
        .await?;
    if let Some(path) = std::env::var_os("CODEX_USAGE_API_FIXTURE_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&result)?)?;
    }
    assert_eq!(result.contract_version, 1);
    assert_eq!(result.source, "usage-v2");
    assert_eq!(
        result.report["scope"],
        json!({"type":"repository","id":"repo-a"})
    );
    assert_eq!(
        result.report["timeRange"],
        json!({"startMs":0,"endMs":3_000})
    );
    assert_eq!(result.report["counts"]["operations"], 1);
    assert_eq!(result.summary["inputTokens"], 100);
    assert_eq!(result.report["providerTokens"][0]["measuredTokens"], 120);
    assert_eq!(result.snapshot.freshness, "fresh");
    assert_eq!(result.snapshot.pending_entries, 0);
    assert_eq!(result.observations.as_ref().map(Vec::len), Some(1));
    assert!(result.next_records_cursor.is_some());
    assert_eq!(
        result.operations.as_ref().unwrap()[0]["operation_id"],
        "model-operation"
    );
    assert!(result.work_contexts.is_empty());
    assert!(result.operation_bindings.is_empty());
    let review = result.review.as_ref().unwrap();
    assert_eq!(review["totals"]["total_tokens"], 120);
    assert_eq!(review["outcomes"]["coverage"], "unattributed");
    assert_eq!(
        review["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
        120
    );
    assert_eq!(review["outcomes"]["attributed"]["operations"], 0);
    assert_eq!(review["outcomes"]["rows"], json!([]));
    // Startup publication can advance the generation between requests. Restart
    // from the first page only after the exact documented invalidation reply.
    let mut first = result;
    let mut page = None;
    for _ in 0..4 {
        let continuation = LocalUsageSummaryParams {
            records_after: first.next_records_cursor.clone(),
            expected_snapshot: Some(first.snapshot.source_watermark.clone()),
            ..params.clone()
        };
        let id = server
            .send_request(
                "localUsage/summary",
                Some(serde_json::to_value(continuation)?),
            )
            .await?;
        let reply = timeout(
            Duration::from_secs(20),
            server.read_stream_until_response_or_error_message(RequestId::Integer(id)),
        )
        .await??;
        match reply {
            JSONRPCMessage::Response(response) => {
                page = Some(serde_json::from_value::<LocalUsageSummaryResponse>(
                    response.result,
                )?);
                break;
            }
            JSONRPCMessage::Error(error) => {
                assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
                assert_eq!(
                    error.error.message,
                    "local usage snapshot changed; restart pagination"
                );
                first = server
                    .request(|request_id| ClientRequest::LocalUsageSummary {
                        request_id,
                        params: params.clone(),
                    })
                    .await?;
            }
            _ => anyhow::bail!("expected a terminal RPC reply"),
        }
    }
    let page = page.context("publication never settled for a consistent continuation")?;
    assert_eq!(page.observations.as_ref().unwrap()[0]["id"], "finished");
    for filter in [
        LocalUsageSummaryParams {
            account_id: Some("account-a".into()),
            ..Default::default()
        },
        LocalUsageSummaryParams {
            thread_id: Some("thread-a".into()),
            ..Default::default()
        },
    ] {
        let scoped: LocalUsageSummaryResponse = server
            .request(|request_id| ClientRequest::LocalUsageSummary {
                request_id,
                params: filter,
            })
            .await?;
        assert_eq!(scoped.summary["inputTokens"], 100);
        assert_eq!(scoped.report["counts"]["operations"], 1);
    }
    let missing_total: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: LocalUsageSummaryParams {
                repository_id: Some("repo-b".into()),
                from_at_ms: Some(0),
                to_at_ms: Some(3_000),
                include_review: true,
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(missing_total.summary["inputTokens"], 9);
    assert_eq!(missing_total.report["coverage"]["state"], "partial");
    assert_eq!(
        missing_total.review.as_ref().unwrap()["totals"]["total_tokens"],
        Value::Null
    );
    assert_eq!(
        missing_total.report["providerTokens"][0]["measuredTokens"],
        0
    );
    assert_eq!(
        missing_total.report["providerTokens"][0]["unknownObservations"],
        1
    );
    assert_eq!(std::fs::read(legacy)?, b"legacy usage must never be opened");
    Ok(())
}

#[tokio::test]
async fn local_usage_summary_rejects_unsupported_versions_and_unstable_pagination() -> Result<()> {
    let home = TempDir::new()?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized_with_timeout(Duration::from_secs(20))
        .await?;
    for params in [
        json!({"contractVersion":99}),
        json!({"recordLimit":51}),
        json!({"worktreeKeys":[]}),
        json!({"includeReview":true}),
        json!({"repositoryId":"repo-a","threadId":"thread-a"}),
        json!({"fromAt":2,"toAt":1}),
        json!({"recordLimit":1,"recordsAfter":{"observedAtMs":1,"id":"row"}}),
        json!({"expectedSnapshot":"different-snapshot"}),
        json!({"workContextBindings":[{"schemaVersion":"must-not-escape","operationId":"op","capturedAtMs":0,"workContext":{}}]}),
    ] {
        let id = server
            .send_request("localUsage/summary", Some(params))
            .await?;
        let error = timeout(
            Duration::from_secs(20),
            server.read_stream_until_error_message(RequestId::Integer(id)),
        )
        .await??;
        assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
        assert!(!serde_json::to_string(&error)?.contains(home.path().to_str().unwrap()));
        assert!(!serde_json::to_string(&error)?.contains("must-not-escape"));
    }
    let id = server
        .send_request(
            "localUsage/summary",
            Some(json!({"repositoryKey":"missing","fromAt":0,"toAt":1})),
        )
        .await?;
    let result: Value = timeout(Duration::from_secs(20), server.read_response(id)).await??;
    assert_eq!(result["report"]["scope"]["id"], "missing");
    assert_eq!(result["report"]["coverage"]["state"], "unobserved");
    Ok(())
}

#[tokio::test]
async fn local_usage_summary_joins_only_explicit_sidecar_bindings() -> Result<()> {
    let home = TempDir::new()?;
    let mut store = UsageStore::open(home.path())?;
    for (id, total) in [("bound-operation", 120), ("unbound-operation", 15)] {
        let mut fact = UsageRecord::new_millis(format!("{id}-finished"), 2_000, total, 0)
            .with_operation(id)
            .with_repository("repo-a")
            .with_activity("coding", "implementation");
        fact.event = Some("model_request_finished".into());
        fact.started_at_ms = Some(1_000);
        fact.ended_at_ms = Some(2_000);
        fact.duration_ms = Some(1_000);
        fact.outcome = Some(UsageOutcome::Completed);
        fact.coverage = Some("recorded".into());
        fact.provider_total_tokens = Some(total);
        store.append_record(fact)?;
    }
    UsagePublisher::open_at(store.root())?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let sidecar = store.root().join("work-contexts.jsonl");
    let declaration = include_str!("../../fixtures/local_usage/work-contexts.jsonl");
    std::fs::write(&sidecar, declaration)?;
    drop(store);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build()
        .await?;
    let init = timeout(
        Duration::from_secs(20),
        server.initialize_with_client_info(ClientInfo {
            name: "usage-v2-binding-test".into(),
            title: None,
            version: "1".into(),
        }),
    )
    .await??;
    let JSONRPCMessage::Response(init) = init else {
        anyhow::bail!("initialize did not return a response");
    };
    assert_eq!(
        init.result["localUsageAccounting"]["supportsWorkContextBindings"],
        true
    );
    let params = LocalUsageSummaryParams {
        repository_id: Some("repo-a".into()),
        from_at_ms: Some(0),
        to_at_ms: Some(3_000),
        include_review: true,
        ..Default::default()
    };
    let result: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: params.clone(),
        })
        .await?;
    if let Some(path) = std::env::var_os("CODEX_USAGE_API_FIXTURE_OUTPUT") {
        std::fs::write(
            std::path::PathBuf::from(path).with_file_name("usage-v2-api-bound-response.json"),
            serde_json::to_vec_pretty(&result)?,
        )?;
    }
    assert_eq!(result.operation_bindings.len(), 1);
    assert_eq!(result.operation_bindings[0].operation_id, "bound-operation");
    assert_eq!(
        result.work_contexts[0].outcome_id.as_deref(),
        Some("outcome-one")
    );
    let review = result.review.as_ref().unwrap();
    assert_eq!(review["totals"]["total_tokens"], 135);
    assert_eq!(review["outcomes"]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(review["outcomes"]["rows"][0]["outcomeId"], "outcome-one");
    assert_eq!(review["outcomes"]["rows"][0]["workstreamId"], "stream-one");
    assert_eq!(review["outcomes"]["attributed"]["operations"], 1);
    assert_eq!(
        review["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        120
    );
    assert_eq!(
        review["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
        15
    );
    assert_eq!(review["outcomes"]["coverage"], "partial");
    assert_eq!(result.report["workBindings"]["boundOperations"], 1);
    assert!(result.report["workBindings"].get("sourceDigest").is_none());
    assert!(
        result.report["workBindings"]["sourceRevision"]
            .as_str()
            .unwrap()
            .starts_with("b1-2000-r")
    );
    assert_eq!(std::fs::read_to_string(&sidecar)?, declaration);
    // The retained view pins compatibility declarations too. A fresh request
    // observes changed bytes, while a continuation keeps the original view.
    std::fs::write(&sidecar, declaration.replace("outcome-one", "outcome-two"))?;
    let id = server
        .send_request(
            "localUsage/summary",
            Some(json!({"expectedSnapshot":result.snapshot.source_watermark,"fromAtMs":0,"toAtMs":3000,"includeReview":true,"repositoryId":"repo-a"})),
        )
        .await?;
    let pinned: LocalUsageSummaryResponse =
        timeout(Duration::from_secs(20), server.read_response(id)).await??;
    assert_eq!(
        pinned.work_contexts[0].outcome_id.as_deref(),
        Some("outcome-one")
    );
    let fresh: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert_eq!(
        fresh.work_contexts[0].outcome_id.as_deref(),
        Some("outcome-two")
    );
    assert_ne!(
        fresh.snapshot.source_watermark,
        pinned.snapshot.source_watermark
    );
    // A conflicting duplicate and unknown content are never partially accepted.
    for invalid in [
        format!(
            "{declaration}{}",
            declaration.replace("outcome-one", "outcome-two")
        ),
        declaration.replace("\"schemaVersion\":1", "\"schemaVersion\":2"),
        declaration.replace(
            "\"workContext\":",
            "\"privateContent\":\"must-not-escape\",\"workContext\":",
        ),
        "x".repeat(512 * 1024 + 1),
    ] {
        std::fs::write(&sidecar, invalid)?;
        let id = server
            .send_request(
                "localUsage/summary",
                Some(json!({"includeReview":true,"fromAtMs":0,"toAtMs":3000})),
            )
            .await?;
        let error = timeout(
            Duration::from_secs(20),
            server.read_stream_until_error_message(RequestId::Integer(id)),
        )
        .await??;
        assert_eq!(error.error.code, -32603);
        assert_eq!(error.error.message, "local usage reader unavailable");
        let encoded = serde_json::to_string(&error)?;
        assert!(!encoded.contains(home.path().to_str().unwrap()));
        assert!(!encoded.contains("must-not-escape"));
    }
    Ok(())
}
