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
                CASE WHEN i IN (32768, 32769) THEN 'local_tool' ELSE 'activity_control' END,
                1000 + i, 1, 'implementation', 'coding',
                'tool_active', 'agent_declared'
         FROM n",
    )
    .bind(process.as_string())
    .bind(child_agent.as_str())
    .execute(&store.pool)
    .await
    .expect("bulk operations");
    sqlx::query(
        "INSERT INTO tool_invocations(
             id, operation_id, operation_kind, tool_kind, safe_tool_name,
             operation_family, observation_timing, covering_model_request_id,
             execution_group_id, execution_role
         ) VALUES
             ('20000000-0000-4000-8000-000000008000', '10000000-0000-4000-8000-000000008000',
              'local_tool', 'builtin', 'exec', 'exec', 'dispatch', NULL,
              '30000000-0000-4000-8000-000000000000', 'wrapper'),
             ('20000000-0000-4000-8000-000000008001', '10000000-0000-4000-8000-000000008001',
              'local_tool', 'builtin', 'exec', 'exec', 'dispatch', NULL,
              '30000000-0000-4000-8000-000000000000', 'nested')",
    )
    .execute(&store.pool)
    .await
    .expect("bulk tool invocations");

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
    assert_eq!(summary.counts.deduplicated_operations, 32_999);
    assert_eq!(summary.counts.model_requests, 0);
    assert_eq!(summary.counts.raw_tool_operations, 2);
    assert_eq!(summary.counts.deduplicated_tool_operations, 1);
    assert_eq!(summary.counts.wrapper_tool_operations, 1);
    assert_eq!(summary.counts.nested_tool_operations, 1);
    assert_eq!(summary.totals.operations, 32_999);
}
