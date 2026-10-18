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
        store
            .ensure_thread(&NewThread {
                id: thread.clone(),
                parent_thread_id: Some(root.clone()),
                source_kind: ThreadSourceKind::new("subagent").expect("source"),
                created_at_ms: 900,
            })
            .await
            .expect("child");
        store
            .ensure_agent(&NewAgent {
                id: agent.clone(),
                thread_id: thread.clone(),
                parent_agent_id: Some(root_agent.clone()),
                role_kind: AgentRoleKind::new("worker").expect("role"),
                created_at_ms: 900,
            })
            .await
            .expect("agent");
        let op = operation(
            process,
            &thread,
            &agent,
            OperationKind::ModelRequest,
            ActivityState::ModelActive,
            /*started_at_ms*/ 1_000,
        );
        let request = record_model_request(&store, &op, Some((1, 2, 3))).await;
        record_total_tokens(
            &store,
            TokenObservationSource::ModelRequest(request),
            FactEventId::new(),
            /*count*/ 10,
            /*observed_at_ms*/ 1_500,
        )
        .await;
        finish(
            &store,
            &op,
            /*ended_at_ms*/ 2_000,
            TerminalStatus::Completed,
            /*error_category*/ None,
        )
        .await;
    }
    let summary = store
        .task_tree_summary(TaskTreeSummaryQuery {
            root_thread_id: root,
            include_descendants: true,
            time_range: UtcTimeRange::new(/*start_ms*/ 900, /*end_ms*/ 2_001).expect("range"),
        })
        .await
        .expect("summary")
        .expect("tree");
    assert_eq!(
        (
            summary.counts.threads,
            summary.counts.agents,
            summary.agents.len()
        ),
        (143, 141, 141)
    );
    assert_eq!(
        summary.totals,
        TaskTreeEffort {
            operations: 141,
            model_requests: 141,
            provider_total_tokens: tokens(/*measured_tokens*/ 1_410),
            wall_time: duration(1_000 * NS_PER_MS)
        }
    );
}

#[tokio::test]
async fn task_tree_summary_handles_more_than_sqlite_variable_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let (process, root, _, _, child_agent) = register_tree(&store).await;
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 32999)
         INSERT INTO operations(
             id, process_id, thread_id, agent_id, operation_kind, started_at_ms,
             taxonomy_version, phase, activity, activity_state, attribution_provenance
         )
         SELECT printf('10000000-0000-4000-8000-%012x', i), ?, 'child-task', ?,
                'activity_control', 1000 + i, 1, 'implementation', 'coding',
                'tool_active', 'agent_declared'
         FROM n",
    )
    .bind(process.as_string())
    .bind(child_agent.as_str())
    .execute(&store.pool)
    .await
    .expect("bulk operations");

    let summary = store
        .task_tree_summary(TaskTreeSummaryQuery {
            root_thread_id: root,
            include_descendants: true,
            time_range: UtcTimeRange::new(/*start_ms*/ 0, /*end_ms*/ 40_000).expect("range"),
        })
        .await
        .expect("summary")
        .expect("tree");
    assert_eq!(summary.counts.threads, 2);
    assert_eq!(summary.counts.agents, 1);
    assert_eq!(summary.counts.raw_operations, 33_000);
    assert_eq!(summary.counts.deduplicated_operations, 33_000);
    assert_eq!(summary.counts.model_requests, 0);
}
