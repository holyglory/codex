use super::*;
use crate::UsageApiEquivalentCost;
use crate::OperationWorkContext;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn cost_receipts_preserve_subsets_late_facts_unknown_models_and_long_context() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store.register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900).await.expect("process");
    insert_thread(&store, "cost").await;
    let mut corrected_operation = None;
    let mut covered_receipt = None;
    for (index, model, values) in [
        (0, "gpt-6-astra", [1_000,300,200,100,1_100,40]),
        (1, "gpt-6-astra", [300_000,100_000,100_000,100,300_100,10]),
        (2, "gpt-6-astra-unknown-alias", [1_000,300,200,100,1_100,40]),
    ] {
        let mut op = operation(process, "cost", OperationKind::ModelRequest);
        if index < 2 {
            op.work_context = Some(OperationWorkContext {
                native_project_id:Some("test-project".to_string()),workstream_id:Some("usage-rollups".to_string()),
                outcome_id:Some(if index == 0 {"outcome-a"} else {"outcome-b"}.to_string()),experiment_ref:None,
            });
        }
        if index == 0 { corrected_operation = Some(op.id); }
        store.begin_operation(&op).await.expect("operation");
        let request = ModelRequestId::new();
        store.record_model_request(&NewModelRequest {
            id:request, operation_id:op.id, provider_kind:ProviderKind::new("openai").expect("provider"),
            model:ModelName::new(model).expect("model"),transport_kind:TransportKind::new("sse").expect("transport"),
            attempt_number:1,account:AccountAttributionSnapshot::unknown(),client_origin:ClientOrigin::new("test").expect("origin"),
        }).await.expect("request");
        let source = FactEventId::new();
        if index == 0 { covered_receipt = Some((request,source)); }
        let mut late = None;
        for (category, value) in ["input_tokens","input_tokens_details.cached_tokens","input_tokens_details.cache_write_tokens","output_tokens","total_tokens","output_tokens_details.reasoning_tokens"].into_iter().zip(values) {
            let mut observation = token(request, RepositoryBucket::Unknown, Some(value), CoverageState::Complete);
            observation.source_event_id = source;
            observation.category_path = TokenCategoryPath::new(category).expect("category");
            if category == "input_tokens_details.cache_write_tokens" {
                late = Some(observation);
            } else {
                store.record_token_observation(&observation).await.expect("component");
            }
        }
        if index == 0 {
            let report = store.usage_summary(UsageSummaryScope::All).await.expect("partial receipt");
            let cost = report.cost.expect("cost coverage");
            assert_eq!((cost.status,cost.estimated_usd_micros,cost.unknown_observations),("unavailable",None,1));
        }
        let late = late.expect("cache write");
        store.record_token_observation(&late).await.expect("late component");
        let report = store.usage_summary(UsageSummaryScope::All).await.expect("cost");
        store.record_token_observation(&late).await.expect("idempotent replay");
        assert_eq!(store.usage_summary(UsageSummaryScope::All).await.expect("replay summary"),report);
        if index == 0 {
            assert_eq!(report.cost,Some(UsageApiEquivalentCost {
                basis:"api_equivalent",currency:"USD",status:"complete",processing_tier:"standard",
                estimated_usd_micros:Some(12_800),input_usd_micros:Some(5_000),cached_input_usd_micros:Some(300),cache_write_usd_micros:Some(2_500),output_usd_micros:Some(5_000),
                input_tokens:1_000,uncached_input_tokens:500,cached_input_tokens:300,cache_write_tokens:200,output_tokens:100,reasoning_tokens:40,provider_total_tokens:1_100,
                priced_observations:1,unknown_observations:0,rate_card_refs:vec!["openai-standard-2026-09-29:gpt-6-astra:short".to_string()],
            }));
        }
    }
    store.record_classification(&NewClassificationEvent {
        event_id:FactEventId::new(),operation_id:corrected_operation.expect("first operation"),
        phase:Phase::Testing,activity:Activity::IntegrationTesting,activity_state:ActivityState::ModelActive,
        provenance:AttributionProvenance::UserCorrected,supersedes_event_id:None,occurred_at_ms:1_300,
    }).await.expect("classification correction");
    let matrix: Vec<(String,String,String,i64)> = sqlx::query_as("SELECT model,outcome_id,activity,SUM(measured_tokens) FROM _usage_report_dimension_tokens WHERE category_path = 'total_tokens' GROUP BY model,outcome_id,activity ORDER BY model,outcome_id,activity")
        .fetch_all(&store.pool).await.expect("dimension rollups");
    assert_eq!(matrix,vec![
        ("gpt-6-astra".to_string(),"outcome-a".to_string(),"integration_testing".to_string(),1_100),
        ("gpt-6-astra".to_string(),"outcome-b".to_string(),"coding".to_string(),300_100),
        ("gpt-6-astra-unknown-alias".to_string(),String::new(),"coding".to_string(),1_100),
    ]);
    let (covering_request,covering_source) = covered_receipt.expect("covered model receipt");
    let mut tool_op = operation(process,"cost",OperationKind::HostedTool);
    tool_op.parent_operation_id = corrected_operation;
    store.begin_operation(&tool_op).await.expect("hosted operation");
    let tool = ToolInvocationId::new();
    store.record_tool_invocation(&NewToolInvocation {
        id:tool,operation_id:tool_op.id,operation_kind:OperationKind::HostedTool,
        tool_kind:ToolKind::new("hosted").expect("kind"),safe_tool_name:ToolName::new("test_tool").expect("name"),
        operation_family:OperationFamily::new("test").expect("family"),observation_timing:ObservationTiming::new("runtime").expect("timing"),
        covering_model_request_id:Some(covering_request),execution_group_id:None,execution_role:ToolExecutionRole::Standalone,
    }).await.expect("hosted tool");
    for (source,count) in [(FactEventId::new(),7),(covering_source,1_100)] {
        let mut observation = token(covering_request,RepositoryBucket::Unknown,Some(count),CoverageState::Complete);
        observation.source = TokenObservationSource::ToolInvocation(tool);
        observation.source_event_id = source;
        observation.category_path = TokenCategoryPath::new("total_tokens").expect("total");
        store.record_token_observation(&observation).await.expect("hosted usage");
    }
    let report = store.usage_summary(UsageSummaryScope::All).await.expect("combined");
    let cost = report.cost.as_ref().expect("cost");
    assert_eq!((cost.status,cost.estimated_usd_micros,cost.priced_observations,cost.unknown_observations),("partial",Some(4_720_300),2,2));
    assert_eq!(cost.provider_total_tokens,302_307);
    sqlx::query("DELETE FROM _usage_report_cache_meta").execute(&store.pool).await.expect("canonical comparison");
    assert_eq!(store.usage_summary(UsageSummaryScope::All).await.expect("raw cost receipts"),report);
    store.close().await;
    let reopened = UsageStore::open(temp.path()).await.expect("rebuild cost receipts");
    assert_eq!(reopened.usage_summary(UsageSummaryScope::All).await.expect("rebuilt"),report);
}
