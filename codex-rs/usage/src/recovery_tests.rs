use super::*;
use crate::Activity;
use crate::ActivityState;
use crate::AttributionProvenance;
use crate::NewOperation;
use crate::OperationKind;
use crate::Phase;
use crate::TerminalOperation;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn recovery_interrupts_only_affected_operations_and_accepts_late_terminals() {
    let home = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(home.path()).await.expect("usage store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 100)
        .await
        .expect("register process");
    let affected = operation(process_id, /*started_at_ms*/ 200);
    let concurrent = operation(process_id, /*started_at_ms*/ 201);
    store
        .begin_operation(&affected)
        .await
        .expect("affected operation");
    store
        .begin_operation(&concurrent)
        .await
        .expect("concurrent operation");

    assert_eq!(
        store
            .recover_after_write_failure(process_id, &[affected.id], /*occurred_at_ms*/ 300)
            .await
            .expect("recover write failure"),
        1
    );
    assert_eq!(
        store.doctor().await.expect("doctor").incomplete_operations,
        1
    );

    store
        .finish_operation(&TerminalOperation {
            operation_id: affected.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 301,
            duration_ns: 5,
            error_category: None,
        })
        .await
        .expect("late terminal is covered by conservative recovery");
    store
        .finish_operation(&TerminalOperation {
            operation_id: concurrent.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 302,
            duration_ns: 6,
            error_category: None,
        })
        .await
        .expect("concurrent terminal");

    assert_eq!(
        store
            .doctor()
            .await
            .expect("final doctor")
            .incomplete_operations,
        0
    );
    assert_eq!(
        sqlx::query_as::<_, (String, Option<i64>, Option<String>)>(
            "SELECT event_kind, duration_ns, error_category FROM operation_events WHERE operation_id = ?",
        )
        .bind(affected.id.as_string())
        .fetch_one(&store.pool)
        .await
        .expect("recovered terminal"),
        (
            "interrupted".to_string(),
            None,
            Some("unavailable".to_string()),
        )
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT reason_code FROM coverage_events WHERE operation_id = ?",
        )
        .bind(affected.id.as_string())
        .fetch_one(&store.pool)
        .await
        .expect("recovery coverage"),
        "write_failure"
    );
}

fn operation(process_id: ProcessId, started_at_ms: i64) -> NewOperation {
    NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: None,
        turn_id: None,
        agent_id: None,
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind: OperationKind::ModelRequest,
        started_at_ms,
        phase: Phase::Unattributed,
        activity: Activity::Unknown,
        activity_state: ActivityState::ModelActive,
        attribution_provenance: AttributionProvenance::Unknown,
    }
}
