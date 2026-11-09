use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server::INVALID_PARAMS_ERROR_CODE;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::RequestId;
use codex_usage::JournalReadLimits;
use codex_usage::UsageOutcome;
use codex_usage::UsagePublisher;
use codex_usage::UsageRecord;
use codex_usage::UsageStore;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

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
        json!({"workstreamId":"stream-one"}),
        json!({"fromAt":2,"toAt":1}),
        json!({"recordLimit":1,"recordsAfter":{"observedAtMs":1,"id":"row"}}),
        json!({"expectedSnapshot":"different-snapshot"}),
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
    // Same line count/timestamp, changed bytes: internal hashing must still
    // invalidate the public readable continuation revision.
    std::fs::write(&sidecar, declaration.replace("outcome-one", "outcome-two"))?;
    let id = server
        .send_request(
            "localUsage/summary",
            Some(json!({"expectedSnapshot":result.snapshot.source_watermark})),
        )
        .await?;
    let error = timeout(
        Duration::from_secs(20),
        server.read_stream_until_error_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(error.error.code, INVALID_PARAMS_ERROR_CODE);
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
            .send_request("localUsage/summary", Some(json!({"includeReview":true})))
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
