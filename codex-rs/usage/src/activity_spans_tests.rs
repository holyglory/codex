use super::*;
use crate::Activity;
use crate::AgentId;
use crate::AttributionProvenance;
use crate::NewOperation;
use crate::OperationKind;
use crate::Phase;
use crate::ProcessId;
use crate::ThreadId;

#[cfg(unix)]
#[tokio::test]
async fn activity_span_facts_are_append_only_and_exactly_replayable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 0)
        .await
        .expect("process");
    sqlx::query(
        "INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms) VALUES ('span-thread', NULL, 'test', 0)",
    )
    .execute(&store.pool)
    .await
    .expect("thread");
    sqlx::query(
        "INSERT INTO agents(id, thread_id, parent_agent_id, role_kind, created_at_ms) VALUES ('span-agent', 'span-thread', NULL, 'root', 0)",
    )
    .execute(&store.pool)
    .await
    .expect("agent");
    let operation = NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: Some(ThreadId::new("span-thread").expect("thread")),
        turn_id: None,
        agent_id: Some(AgentId::new("span-agent").expect("agent")),
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind: OperationKind::LocalTool,
        started_at_ms: 1,
        phase: Phase::Planning,
        activity: Activity::Coordination,
        activity_state: ActivityState::ToolActive,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    };
    store.begin_operation(&operation).await.expect("operation");
    let span = NewActivitySpan {
        id: ActivitySpanId::new(),
        operation_id: operation.id,
        activity_state: ActivityState::BlockedWait,
        started_at_ms: 2,
    };
    store.begin_activity_span(&span).await.expect("span");
    store.begin_activity_span(&span).await.expect("span replay");
    let event = NewActivitySpanEvent {
        event_id: FactEventId::new(),
        activity_span_id: span.id,
        kind: ActivitySpanEventKind::Heartbeat,
        occurred_at_ms: 3,
    };
    store
        .record_activity_span_event(&event)
        .await
        .expect("heartbeat");
    store
        .record_activity_span_event(&event)
        .await
        .expect("heartbeat replay");
    let mut conflict = event;
    conflict.occurred_at_ms = 4;
    assert!(matches!(
        store.record_activity_span_event(&conflict).await,
        Err(UsageStoreError::FactConflict)
    ));
    let ended = NewActivitySpanEvent {
        event_id: FactEventId::new(),
        activity_span_id: span.id,
        kind: ActivitySpanEventKind::Ended,
        occurred_at_ms: 5,
    };
    store.record_activity_span_event(&ended).await.expect("end");
    store
        .record_activity_span_event(&ended)
        .await
        .expect("end replay");
    assert!(matches!(
        store
            .record_activity_span_event(&NewActivitySpanEvent {
                event_id: FactEventId::new(),
                occurred_at_ms: 6,
                ..ended
            })
            .await,
        Err(UsageStoreError::FactConflict)
    ));
    assert!(
        sqlx::query("UPDATE activity_spans SET started_at_ms = 9 WHERE id = ?")
            .bind(span.id.as_string())
            .execute(&store.pool)
            .await
            .is_err()
    );
}
