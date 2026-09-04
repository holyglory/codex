use super::*;
#[cfg(unix)]
use crate::types::Activity;
#[cfg(unix)]
use crate::types::ActivityState;
#[cfg(unix)]
use crate::types::AttributionProvenance;
#[cfg(unix)]
use crate::types::MeasurementProvenance;
#[cfg(unix)]
use crate::types::OperationId;
#[cfg(unix)]
use crate::types::OperationKind;
#[cfg(unix)]
use crate::types::Phase;
#[cfg(unix)]
use crate::types::TerminalStatus;
#[cfg(unix)]
use crate::types::TokenUnit;
#[cfg(unix)]
use codex_state::SqlitePoolProfile;
#[cfg(unix)]
use codex_state::open_sqlite_pool;
#[cfg(unix)]
use pretty_assertions::assert_eq;
use serde::Deserialize;
#[cfg(unix)]
use sqlx::migrate::Migrate;
#[cfg(unix)]
use uuid::Uuid;

#[tokio::test]
async fn private_store_workflow_protects_database_and_sidecars() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    sqlx::query("CREATE TABLE IF NOT EXISTS private_storage_probe(id INTEGER PRIMARY KEY)")
        .execute(&store.pool)
        .await
        .expect("write probe");
    let usage_dir = temp.path().join("usage");
    let database = usage_dir.join("usage.sqlite3");
    codex_private_storage::verify_private_directory(&usage_dir).expect("private usage directory");
    codex_private_storage::verify_private_file(&database).expect("private database");
    for suffix in ["-wal", "-shm"] {
        let sidecar = usage_dir.join(format!("usage.sqlite3{suffix}"));
        if sidecar.exists() {
            codex_private_storage::verify_private_file(&sidecar).expect("private sidecar");
        }
    }
}

#[tokio::test]
async fn repository_lookup_index_preserves_history_across_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let before = store.database_schema_version().await.expect("schema");
    let plan: Vec<String> = sqlx::query("EXPLAIN QUERY PLAN SELECT operation_id FROM repository_attributions WHERE repository_id = 'fixture'")
        .fetch_all(&store.pool).await.expect("query plan")
        .into_iter().map(|row| row.get::<String, _>("detail")).collect();
    assert!(
        plan.iter().any(|detail| detail
            .contains("USING COVERING INDEX repository_attributions_repository_operation_idx")),
        "{plan:?}"
    );
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 1, /*started_at_ms*/ 1)
        .await
        .expect("record process");
    drop(store);
    let reopened = UsageStore::open(temp.path()).await.expect("reopen");
    assert_eq!(
        reopened.database_schema_version().await.expect("schema"),
        before
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM process_instances")
        .fetch_one(&reopened.pool)
        .await
        .expect("preserved history");
    assert_eq!(count, 1);
}

#[cfg(unix)]
fn operation(process_id: &ProcessId) -> NewOperation {
    NewOperation {
        id: OperationId::new(),
        process_id: *process_id,
        thread_id: None,
        turn_id: None,
        agent_id: None,
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind: OperationKind::ModelRequest,
        started_at_ms: 1_000,
        phase: Phase::Planning,
        activity: Activity::RepositoryAnalysis,
        activity_state: ActivityState::ModelActive,
        attribution_provenance: AttributionProvenance::AgentDeclared,
    }
}

#[cfg(unix)]
async fn insert_thread_graph(store: &UsageStore) {
    sqlx::query(
        "INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms) VALUES (?, NULL, 'cli', 1)",
    )
    .bind("thread-a")
    .execute(&store.pool)
    .await
    .expect("insert thread a");
    sqlx::query(
        "INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms) VALUES (?, 'thread-a', 'cli', 1)",
    )
    .bind("thread-b")
    .execute(&store.pool)
    .await
    .expect("insert thread b");
    sqlx::query(
        "INSERT INTO agents(id, thread_id, parent_agent_id, role_kind, created_at_ms) VALUES (?, ?, NULL, 'root', 1)",
    )
    .bind("agent-a")
    .bind("thread-a")
    .execute(&store.pool)
    .await
    .expect("insert agent a");
    sqlx::query(
        "INSERT INTO turns(id, thread_id, account_profile_ref, created_at_ms) VALUES (?, ?, NULL, 1)",
    )
    .bind("turn-b")
    .bind("thread-b")
    .execute(&store.pool)
    .await
    .expect("insert turn b");
    sqlx::query(
        "INSERT INTO agents(id, thread_id, parent_agent_id, role_kind, created_at_ms) VALUES (?, ?, 'agent-a', 'delegated', 1)",
    )
    .bind("agent-b")
    .bind("thread-b")
    .execute(&store.pool)
    .await
    .expect("insert agent b");
}

#[cfg(unix)]
async fn insert_model_request(store: &UsageStore, operation_id: OperationId, request_id: &str) {
    sqlx::query(
        r#"
        INSERT INTO model_requests(
            id, operation_id, operation_kind, provider_kind, model,
            transport_kind, attempt_number, client_origin
        ) VALUES (?, ?, 'model_request', 'openai', 'test-model', 'sse', 1, 'test')
        "#,
    )
    .bind(request_id)
    .bind(operation_id.as_string())
    .execute(&store.pool)
    .await
    .expect("insert model request");
}

#[cfg(unix)]
#[tokio::test]
async fn operation_lifecycle_is_idempotent_and_doctor_reports_completion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    let operation = operation(&process_id);
    store
        .begin_operation(&operation)
        .await
        .expect("begin operation");
    store
        .begin_operation(&operation)
        .await
        .expect("repeat begin");
    let terminal = TerminalOperation {
        operation_id: operation.id,
        status: TerminalStatus::Completed,
        occurred_at_ms: 1_100,
        duration_ns: 100,
        error_category: None,
    };
    store
        .finish_operation(&terminal)
        .await
        .expect("finish operation");
    store
        .finish_operation(&terminal)
        .await
        .expect("repeat finish");

    assert_eq!(
        DoctorReport {
            integrity: "ok".to_string(),
            migration_count: 5,
            incomplete_operations: 0,
        },
        store.doctor().await.expect("doctor")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn conflicting_replays_are_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    assert!(matches!(
        store
            .register_process(&process_id, /*os_pid*/ 43, /*started_at_ms*/ 900)
            .await,
        Err(UsageStoreError::ProcessConflict)
    ));
    let operation = operation(&process_id);
    store
        .begin_operation(&operation)
        .await
        .expect("begin operation");
    let mut conflicting = operation.clone();
    conflicting.activity = Activity::Coding;
    assert!(matches!(
        store.begin_operation(&conflicting).await,
        Err(UsageStoreError::OperationConflict)
    ));

    let terminal = TerminalOperation {
        operation_id: operation.id,
        status: TerminalStatus::Completed,
        occurred_at_ms: 1_100,
        duration_ns: 100,
        error_category: None,
    };
    store
        .finish_operation(&terminal)
        .await
        .expect("finish operation");
    let mut conflicting_terminal = terminal.clone();
    conflicting_terminal.status = TerminalStatus::Failed;
    assert!(matches!(
        store.finish_operation(&conflicting_terminal).await,
        Err(UsageStoreError::TerminalConflict)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn append_only_history_rejects_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    let operation = operation(&process_id);
    store
        .begin_operation(&operation)
        .await
        .expect("begin operation");
    store
        .finish_operation(&TerminalOperation {
            operation_id: operation.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 1_100,
            duration_ns: 100,
            error_category: None,
        })
        .await
        .expect("finish operation");

    let error = sqlx::query("DELETE FROM operation_events WHERE operation_id = ?")
        .bind(operation.id.as_string())
        .execute(&store.pool)
        .await
        .expect_err("event deletion should fail");
    assert!(error.to_string().contains("usage events cannot be deleted"));
}

#[test]
fn process_id_deserialization_rejects_non_uuid_content() {
    let deserializer = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(
        "content-that-must-not-be-stored",
    );
    let result = ProcessId::deserialize(deserializer);
    assert!(result.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn process_lifecycle_is_append_only_and_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    store
        .heartbeat_process(&process_id, /*occurred_at_ms*/ 1_000)
        .await
        .expect("heartbeat");
    store
        .heartbeat_process(&process_id, /*occurred_at_ms*/ 1_000)
        .await
        .expect("replay heartbeat");
    store
        .finish_process(&process_id, /*occurred_at_ms*/ 1_100)
        .await
        .expect("finish process");
    store
        .finish_process(&process_id, /*occurred_at_ms*/ 1_100)
        .await
        .expect("replay finish");
    store
        .heartbeat_process(&process_id, /*occurred_at_ms*/ 1_000)
        .await
        .expect("replay heartbeat after finish");

    assert!(matches!(
        store
            .finish_process(&process_id, /*occurred_at_ms*/ 1_101)
            .await,
        Err(UsageStoreError::ProcessEventConflict)
    ));
    assert!(matches!(
        store
            .heartbeat_process(&process_id, /*occurred_at_ms*/ 1_050)
            .await,
        Err(UsageStoreError::Database(_))
    ));
    let process_key = process_id.as_string();
    assert!(
        sqlx::query("UPDATE process_instances SET os_pid = 43 WHERE id = ?")
            .bind(&process_key)
            .execute(&store.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM process_instances WHERE id = ?")
            .bind(&process_key)
            .execute(&store.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM process_events WHERE process_id = ?")
            .bind(&process_key)
            .execute(&store.pool)
            .await
            .is_err()
    );
    assert_eq!(
        2,
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM process_events WHERE process_id = ?")
            .bind(process_key)
            .fetch_one(&store.pool)
            .await
            .expect("process event count")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn operation_relationships_and_kinds_are_database_enforced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    insert_thread_graph(&store).await;

    assert!(
        sqlx::query(
            r#"
            INSERT INTO agents(id, thread_id, parent_agent_id, role_kind, created_at_ms)
            VALUES ('agent-cross-thread', 'thread-a', 'agent-b', 'delegated', 1)
            "#,
        )
        .execute(&store.pool)
        .await
        .is_err()
    );

    let mut wrong_turn = operation(&process_id);
    wrong_turn.thread_id = Some(crate::types::ThreadId::new("thread-a").expect("thread"));
    wrong_turn.turn_id = Some(crate::types::TurnId::new("turn-b").expect("turn"));
    assert!(matches!(
        store.begin_operation(&wrong_turn).await,
        Err(UsageStoreError::Database(_))
    ));

    let mut wrong_agent = operation(&process_id);
    wrong_agent.thread_id = Some(crate::types::ThreadId::new("thread-a").expect("thread"));
    wrong_agent.agent_id = Some(crate::types::AgentId::new("agent-b").expect("agent"));
    assert!(matches!(
        store.begin_operation(&wrong_agent).await,
        Err(UsageStoreError::Database(_))
    ));

    let mut local_tool = operation(&process_id);
    local_tool.kind = OperationKind::LocalTool;
    store
        .begin_operation(&local_tool)
        .await
        .expect("begin local tool");
    assert!(
        sqlx::query(
            r#"
            INSERT INTO model_requests(
                id, operation_id, operation_kind, provider_kind, model,
                transport_kind, attempt_number, client_origin
            ) VALUES ('request-wrong-kind', ?, 'model_request', 'openai', 'test', 'sse', 1, 'test')
            "#,
        )
        .bind(local_tool.id.as_string())
        .execute(&store.pool)
        .await
        .is_err()
    );

    let model_request = operation(&process_id);
    store
        .begin_operation(&model_request)
        .await
        .expect("begin model request");
    assert!(
        sqlx::query(
            r#"
            INSERT INTO tool_invocations(
                id, operation_id, operation_kind, tool_kind, safe_tool_name,
                operation_family, observation_timing
            ) VALUES ('tool-wrong-kind', ?, 'local_tool', 'builtin', 'test', 'other', 'runtime')
            "#,
        )
        .bind(model_request.id.as_string())
        .execute(&store.pool)
        .await
        .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn token_observations_preserve_unknown_counts_and_dedupe_replay() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    let request_operation = operation(&process_id);
    store
        .begin_operation(&request_operation)
        .await
        .expect("begin request");
    insert_model_request(&store, request_operation.id, "request-1").await;
    let source_event_id = Uuid::now_v7().to_string();
    let insert = |observation_id: String| {
        sqlx::query(
            r#"
            INSERT INTO token_observations(
                id, model_request_id, tool_invocation_id, source_event_id,
                category_path, token_count, unit, measurement_provenance,
                coverage_state, repository_bucket, observed_at_ms
            ) VALUES (?, 'request-1', NULL, ?, 'input_tokens_details.audio_tokens',
                      NULL, ?, ?, 'unknown', 'unknown', 1_000)
            ON CONFLICT(model_request_id, source_event_id, category_path)
                WHERE model_request_id IS NOT NULL DO NOTHING
            "#,
        )
        .bind(observation_id)
        .bind(&source_event_id)
        .bind(TokenUnit::Tokens.as_str())
        .bind(MeasurementProvenance::ProviderReported.as_str())
    };
    assert_eq!(
        1,
        insert(Uuid::now_v7().to_string())
            .execute(&store.pool)
            .await
            .expect("insert token observation")
            .rows_affected()
    );
    assert_eq!(
        0,
        insert(Uuid::now_v7().to_string())
            .execute(&store.pool)
            .await
            .expect("replay token observation")
            .rows_affected()
    );
    assert_eq!(
        (None, "tokens".to_string(), "provider_reported".to_string()),
        sqlx::query_as::<_, (Option<i64>, String, String)>(
            "SELECT token_count, unit, measurement_provenance FROM token_observations",
        )
        .fetch_one(&store.pool)
        .await
        .expect("stored token observation")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn durable_pool_pragmas_and_taxonomy_catalog_are_present() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&store.pool)
        .await
        .expect("journal mode");
    let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(&store.pool)
        .await
        .expect("synchronous");
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&store.pool)
        .await
        .expect("foreign keys");
    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&store.pool)
        .await
        .expect("busy timeout");
    assert_eq!(
        ("wal".to_string(), 2, 1, 5_000),
        (journal_mode, synchronous, foreign_keys, busy_timeout,)
    );
    assert_eq!(
        (1, 1, "builtin_v1".to_string(), None),
        sqlx::query_as::<_, (i64, i64, String, Option<i64>)>(
            r#"
            SELECT version, schema_migration, mapping_key, supersedes_version
            FROM taxonomy_versions
            "#,
        )
        .fetch_one(&store.pool)
        .await
        .expect("taxonomy version")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn populated_v3_database_migrates_without_losing_account_attribution() {
    let temp = tempfile::tempdir().expect("tempdir");
    let usage_dir = temp.path().join("usage");
    std::fs::create_dir_all(&usage_dir).expect("usage directory");
    let database_path = usage_dir.join("usage.sqlite3");
    let pool = open_sqlite_pool(&database_path, SqlitePoolProfile::DurableEvents)
        .await
        .expect("open v3 fixture database");
    let mut connection = pool.acquire().await.expect("migration connection");
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await
        .expect("migration table");
    for migration in MIGRATOR.iter().take(3) {
        connection
            .apply("_sqlx_migrations", migration)
            .await
            .expect("apply v3 migration");
    }
    drop(connection);
    sqlx::raw_sql(
        r#"
        INSERT INTO process_instances(id, os_pid, started_at_ms) VALUES ('process-v3', 7, 10);
        INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms)
            VALUES ('thread-v3', NULL, 'root', 11);
        INSERT INTO turns(id, thread_id, account_profile_ref, created_at_ms, account_auth_mode)
            VALUES ('turn-v3', 'thread-v3', 'account-v3', 12, 'bedrock_api_key');
        INSERT INTO operations(
            id, process_id, thread_id, turn_id, agent_id, parent_operation_id,
            retry_of_operation_id, rework_of_operation_id, operation_kind, started_at_ms,
            taxonomy_version, phase, activity, activity_state, attribution_provenance
        ) VALUES (
            'operation-v3', 'process-v3', 'thread-v3', 'turn-v3', NULL, NULL,
            NULL, NULL, 'model_request', 13, 1, 'implementation', 'coding',
            'model_active', 'unknown'
        );
        INSERT INTO model_requests(
            id, operation_id, operation_kind, provider_kind, model, transport_kind,
            attempt_number, account_profile_ref, client_origin, account_auth_mode
        ) VALUES (
            'request-v3', 'operation-v3', 'model_request', 'amazon-bedrock', 'model-v3',
            'responses_http', 1, 'account-v3', 'root', 'bedrock_api_key'
        );
        "#,
    )
    .execute(&pool)
    .await
    .expect("populate v3 fixture");
    pool.close().await;
    codex_private_storage::ensure_private_directory(&usage_dir).expect("private usage directory");
    for path in [
        database_path.clone(),
        database_path.with_file_name("usage.sqlite3-wal"),
        database_path.with_file_name("usage.sqlite3-shm"),
    ] {
        if path.exists() {
            codex_private_storage::ensure_private_file(&path).expect("private usage file");
        }
    }

    let store = UsageStore::open(temp.path())
        .await
        .expect("migrate v3 store");
    assert_eq!(store.doctor().await.expect("doctor").migration_count, 5);
    assert_eq!(
        (
            sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT id, thread_id, account_auth_mode FROM turns WHERE id = 'turn-v3'",
            )
            .fetch_one(&store.pool)
            .await
            .expect("preserved turn"),
            sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT id, operation_id, account_auth_mode FROM model_requests WHERE id = 'request-v3'",
            )
            .fetch_one(&store.pool)
            .await
            .expect("preserved request"),
        ),
        (
            ("turn-v3".to_string(), "thread-v3".to_string(), Some("bedrock_api_key".to_string())),
            ("request-v3".to_string(), "operation-v3".to_string(), Some("bedrock_api_key".to_string())),
        )
    );
    sqlx::query(
        "INSERT INTO turns(id, thread_id, account_profile_ref, created_at_ms, account_auth_mode) VALUES ('turn-v4', 'thread-v3', NULL, 14, 'bedrock_access_keys')",
    )
    .execute(&store.pool)
    .await
    .expect("new Bedrock access-key mode");
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&store.pool)
            .await
            .expect("foreign key check")
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn migration_failure_is_preserved_and_display_is_redacted() {
    let temp = tempfile::tempdir().expect("tempdir");
    let usage_dir = temp.path().join("usage");
    std::fs::create_dir_all(&usage_dir).expect("usage directory");
    let database_path = usage_dir.join("usage.sqlite3");
    let pool = open_sqlite_pool(&database_path, SqlitePoolProfile::DurableEvents)
        .await
        .expect("open fixture database");
    sqlx::query("CREATE TABLE process_instances(marker TEXT) STRICT")
        .execute(&pool)
        .await
        .expect("create incompatible schema");
    pool.close().await;

    let error = match UsageStore::open(temp.path()).await {
        Ok(_) => panic!("incompatible migration should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, UsageStoreError::Migration(_)));
    assert_eq!(error.to_string(), "usage database migration failed");
    assert!(
        !error
            .to_string()
            .contains(temp.path().to_string_lossy().as_ref())
    );

    let pool = open_sqlite_pool(&database_path, SqlitePoolProfile::DurableEvents)
        .await
        .expect("reopen preserved database");
    assert_eq!(
        1,
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'process_instances'",
        )
        .fetch_one(&pool)
        .await
        .expect("preserved schema")
    );
    pool.close().await;
}

#[cfg(not(unix))]
#[tokio::test]
async fn unsupported_platform_fails_closed_before_filesystem_access() {
    let error = match UsageStore::open(Path::new("unused")).await {
        Ok(_) => panic!("unsupported platform should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, UsageStoreError::UnsupportedPlatform));
}

#[cfg(unix)]
#[tokio::test]
async fn usage_directory_and_database_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    assert_eq!(
        0o700,
        std::fs::metadata(temp.path().join("usage"))
            .expect("usage metadata")
            .permissions()
            .mode()
            & 0o777
    );
    assert_eq!(
        0o600,
        std::fs::metadata(temp.path().join("usage/usage.sqlite3"))
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o777
    );
    store.close().await;
}
