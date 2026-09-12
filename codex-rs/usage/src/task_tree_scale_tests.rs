use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn large_task_tree_includes_all_descendants_and_agent_totals() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let (process, root, _, root_agent, _) = register_tree(&store).await;
    for index in 0..141 {
        let thread = ThreadId::new(format!("child-{index:03}")).expect("thread");
        let agent = AgentId::new(format!("agent-{index:03}")).expect("agent");
        store.ensure_thread(&NewThread {id:thread.clone(), parent_thread_id:Some(root.clone()),
            source_kind:ThreadSourceKind::new("subagent").expect("source"),created_at_ms:900}).await.expect("child");
        store.ensure_agent(&NewAgent {id:agent.clone(),thread_id:thread.clone(),parent_agent_id:Some(root_agent.clone()),
            role_kind:AgentRoleKind::new("worker").expect("role"),created_at_ms:900}).await.expect("agent");
        let op = operation(process,&thread,&agent,OperationKind::ModelRequest,ActivityState::ModelActive,/*started_at_ms*/ 1_000);
        let request = record_model_request(&store,&op,Some((1,2,3))).await;
        record_total_tokens(&store,TokenObservationSource::ModelRequest(request),FactEventId::new(),/*count*/ 10,/*observed_at_ms*/ 1_500).await;
        finish(&store,&op,/*ended_at_ms*/ 2_000,TerminalStatus::Completed,/*error_category*/ None).await;
    }
    let summary=store.task_tree_summary(TaskTreeSummaryQuery {root_thread_id:root,include_descendants:true,
        time_range:UtcTimeRange::new(900,2_001).expect("range")}).await.expect("summary").expect("tree");
    assert_eq!((summary.counts.threads,summary.counts.agents,summary.agents.len()),(143,141,141));
    assert_eq!(summary.totals,TaskTreeEffort {operations:141,model_requests:141,
        provider_total_tokens:tokens(1_410),wall_time:duration(1_000*NS_PER_MS)});
}
