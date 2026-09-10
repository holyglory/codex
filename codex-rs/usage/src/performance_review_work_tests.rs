use super::*;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migrate;

fn binding(observed_at_ms: i64) -> NewWorkBinding {
    NewWorkBinding {
        thread_id: ThreadId::new("spec").expect("thread"),
        native_project_id: "project-alpha".to_string(),
        workstream_id: Some("spec".to_string()),
        outcome_id: Some("p0b4535bb84b8f5cd".to_string()),
        experiment_ref: None,
        observed_at_ms,
    }
}

#[tokio::test]
async fn before_bind_and_midturn_requests_keep_unknown_attribution() {
    let fixture = Fixture::new().await;
    let mut request = fixture.operation(/*started_at_ms*/ 1_000_000);
    request.kind = OperationKind::ModelRequest;
    let request_id = fixture.model(&request).await;
    fixture
        .store
        .record_work_binding(&binding(/*observed_at_ms*/ 1_000_010))
        .await
        .expect("bind spec");
    fixture
        .token(
            TokenObservationSource::ModelRequest(request_id),
            FactEventId::new(),
            "total_tokens",
            /*count*/ 100,
        )
        .await;
    fixture.finish(&request, /*duration_ms*/ 100).await;
    for started_at_ms in [1_000_010, 1_000_011, 1_000_031] {
        let operation = fixture.operation(started_at_ms);
        fixture.begin(&operation).await;
        fixture.finish(&operation, /*duration_ms*/ 10).await;
    }
    let mut activated = binding(/*observed_at_ms*/ 1_000_030);
    activated.workstream_id = Some("implementation".to_string());
    activated.experiment_ref = Some("review-one@1".to_string());
    fixture
        .store
        .record_work_binding(&activated)
        .await
        .expect("activate implementation");
    let packet = fixture.packet().await;
    assert_eq!(
        (
            packet.work_bindings.bound_operations,
            packet.work_bindings.unknown_operations,
            packet.work_bindings.boundary_operations,
            packet.work_bindings.unknown_workstream_operations
        ),
        (2, 2, 1, 2)
    );
    assert_eq!(
        packet
            .work_bindings
            .references
            .iter()
            .map(|reference| (
                reference.workstream_id.as_deref(),
                reference.bound_operations,
                reference.provenance.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some("implementation"), 1, "runtime_observed"),
            (Some("spec"), 1, "runtime_observed")
        ]
    );
    assert_eq!(packet.tokens[0].exact_tokens, Some(100));
    let owners = sqlx::query_as::<_, (String, String)>(
        "SELECT model_request_id, source_event_id FROM token_observations",
    )
    .fetch_all(&fixture.store.pool)
    .await
    .expect("factual token owners");
    assert_eq!(owners[0].0, request_id.as_string());
    assert_eq!(fixture.packet().await, packet);
}

#[tokio::test]
async fn duplicate_binding_replay_is_idempotent_and_conflicting_overlap_is_unknown() {
    let fixture = Fixture::new().await;
    let first = binding(/*observed_at_ms*/ 1_000_010);
    fixture
        .store
        .record_work_binding(&first)
        .await
        .expect("first bind");
    fixture
        .store
        .record_work_binding(&first)
        .await
        .expect("replay bind");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_bindings")
        .fetch_one(&fixture.store.pool)
        .await
        .expect("count");
    assert_eq!(count, 1);
    let mut conflict = first.clone();
    conflict.native_project_id = "project-beta".to_string();
    fixture
        .store
        .record_work_binding(&conflict)
        .await
        .expect("record actual conflicting observation");
    let ambiguous = fixture.operation(/*started_at_ms*/ 1_000_011);
    fixture.begin(&ambiguous).await;
    let mut resolved = first;
    resolved.observed_at_ms = 1_000_020;
    fixture
        .store
        .record_work_binding(&resolved)
        .await
        .expect("later unambiguous activation");
    let current = fixture.operation(/*started_at_ms*/ 1_000_021);
    fixture.begin(&current).await;
    let packet = fixture.packet().await;
    assert_eq!(
        (
            packet.work_bindings.bound_operations,
            packet.work_bindings.unknown_operations,
            packet.work_bindings.ambiguous_operations
        ),
        (1, 1, 1)
    );
    assert_eq!(
        packet
            .work_bindings
            .references
            .iter()
            .map(|reference| reference.bound_operations)
            .sum::<u64>(),
        1
    );
    assert!(
        sqlx::query("UPDATE work_bindings SET native_project_id = 'replacement'")
            .execute(&fixture.store.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM work_bindings")
            .execute(&fixture.store.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn repository_and_thread_scope_never_infer_another_project_or_split_multi_repo_work() {
    let fixture = Fixture::new().await;
    fixture
        .store
        .record_work_binding(&binding(/*observed_at_ms*/ 1_000_010))
        .await
        .expect("bind");
    let other_repository = fixture
        .store
        .resolve_repository(
            &RepositoryIdentityInput::new(
                CanonicalRepositoryPath::new("/project-beta").expect("path"),
            ),
            &SafeRepositoryLabel::new("project-beta").expect("label"),
            /*observed_at_ms*/ 1_000_020,
        )
        .await
        .expect("other repository");
    let other_thread = ThreadId::new("unbound-thread").expect("thread");
    fixture
        .store
        .ensure_thread(&NewThread {
            id: other_thread.clone(),
            parent_thread_id: None,
            source_kind: ThreadSourceKind::new("test").expect("source"),
            created_at_ms: 1_000_010,
        })
        .await
        .expect("other thread");
    let mut other_operation = fixture.operation(/*started_at_ms*/ 1_000_020);
    other_operation.thread_id = Some(other_thread);
    fixture
        .store
        .begin_operation(&other_operation)
        .await
        .expect("unbound operation");
    fixture
        .store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: FactEventId::new(),
            operation_id: other_operation.id,
            repository_id: Some(other_repository.clone()),
            kind: RepositoryAttributionKind::Primary,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms: 1_000_020,
        })
        .await
        .expect("other repository fact");
    let multi_repo = fixture.operation(/*started_at_ms*/ 1_000_030);
    fixture.begin(&multi_repo).await;
    fixture
        .store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: FactEventId::new(),
            operation_id: multi_repo.id,
            repository_id: Some(other_repository.clone()),
            kind: RepositoryAttributionKind::FileChange,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms: 1_000_030,
        })
        .await
        .expect("actual multi repo fact");
    let packet = fixture
        .store
        .performance_review_packet(PerformanceReviewQuery {
            repository_id: Some(other_repository),
            ..Default::default()
        })
        .await
        .expect("repository packet");
    assert_eq!(
        (
            packet.coverage.deduplicated_operations,
            packet.work_bindings.bound_operations,
            packet.work_bindings.unknown_operations,
            packet.work_bindings.multi_repository_operations
        ),
        (2, 0, 2, 1)
    );
    assert!(
        packet
            .work_bindings
            .references
            .iter()
            .all(|reference| reference.bound_operations == 0)
    );
}

#[tokio::test]
async fn project_only_binding_preserves_unknown_workstream_and_payloads_never_enter_storage() {
    let fixture = Fixture::new().await;
    for invalid in [
        "/private/path",
        "project;command",
        "project\ncontent",
        "",
        &"x".repeat(257),
    ] {
        let mut invalid_binding = binding(/*observed_at_ms*/ 1_000_000);
        invalid_binding.native_project_id = invalid.to_string();
        assert!(matches!(
            fixture.store.record_work_binding(&invalid_binding).await,
            Err(UsageStoreError::InvalidFact)
        ));
    }
    let mut project_only = binding(/*observed_at_ms*/ 1_000_010);
    project_only.workstream_id = None;
    project_only.outcome_id = None;
    fixture
        .store
        .record_work_binding(&project_only)
        .await
        .expect("project-only bind");
    fixture
        .begin(&fixture.operation(/*started_at_ms*/ 1_000_011))
        .await;
    let packet = fixture.packet().await;
    assert_eq!(
        (
            packet.work_bindings.bound_operations,
            packet.work_bindings.unknown_workstream_operations
        ),
        (1, 1)
    );
    assert_eq!(packet.work_bindings.references[0].workstream_id, None);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_bindings")
        .fetch_one(&fixture.store.pool)
        .await
        .expect("stored facts");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn binding_samples_keep_packet_bounded_with_large_tokens_and_long_identifiers() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    let request = fixture.model(&model).await;
    fixture.finish(&model, /*duration_ms*/ 10).await;
    for index in 0..40 {
        let category = format!(
            "{}.{}.category_{index:02}_{}_tokens",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(40)
        );
        fixture
            .token(
                TokenObservationSource::ModelRequest(request),
                FactEventId::new(),
                &category,
                i64::MAX as u64,
            )
            .await;
    }
    for offset in 1..=20 {
        let mut record = binding(1_000_000 + offset * 100);
        record.native_project_id = "p".repeat(256);
        record.workstream_id = Some(format!("{offset:0>256}"));
        record.outcome_id = Some("o".repeat(256));
        record.experiment_ref = Some(format!("{}@4294967295", "e".repeat(245)));
        fixture
            .store
            .record_work_binding(&record)
            .await
            .expect("long binding");
        let mut retry = fixture.operation(record.observed_at_ms + 1);
        retry.retry_of_operation_id = Some(model.id);
        fixture.begin(&retry).await;
        fixture.finish(&retry, /*duration_ms*/ 40).await;
    }
    let started = Instant::now();
    let packet = fixture.packet().await;
    let bytes = serde_json::to_vec(&packet).expect("json").len();
    eprintln!(
        "work_binding measurements: bytes={bytes}, elapsed_us={}, operations=21, bindings=20, token_categories=40",
        started.elapsed().as_micros()
    );
    assert!(
        bytes + 512 <= 12 * 1024,
        "packet and reporting disclosure require {} bytes",
        bytes + 512
    );
    assert!(packet.work_bindings.references.len() <= 8);
    assert_eq!(packet.work_bindings.bound_operations, 20);
    assert_eq!(
        packet.work_bindings.omitted_references + packet.work_bindings.references.len() as u64,
        20
    );
    assert_eq!(packet.coverage.omitted_token_categories, 28);
}

#[tokio::test]
async fn binding_lookup_uses_the_thread_time_index_and_keeps_unrelated_bindings_out() {
    let fixture = Fixture::new().await;
    fixture
        .store
        .record_work_binding(&binding(/*observed_at_ms*/ 1_000_010))
        .await
        .expect("bind");
    let plan: Vec<String> = sqlx::query("EXPLAIN QUERY PLAN SELECT MAX(observed_at_ms) FROM work_bindings WHERE thread_id = 'spec' AND observed_at_ms <= 1000011")
        .fetch_all(&fixture.store.pool).await.expect("query plan").into_iter().map(|row| row.get("detail")).collect();
    assert!(
        plan.iter()
            .any(|line| line.contains("work_bindings_thread_observed_idx")),
        "{plan:?}"
    );
    let future = fixture.operation(/*started_at_ms*/ 1_000_011);
    fixture.begin(&future).await;
    let packet = fixture
        .store
        .performance_review_packet(PerformanceReviewQuery {
            time_range: Some(
                UtcTimeRange::new(/*start_ms*/ 1_000_000, /*end_ms*/ 1_000_010).expect("range"),
            ),
            ..Default::default()
        })
        .await
        .expect("earlier window");
    assert!(packet.work_bindings.references.is_empty());
    assert_eq!(packet.work_bindings.bound_operations, 0);
}

#[tokio::test]
async fn experiment_refs_accept_immutable_review_revisions_without_broadening_other_ids() {
    let fixture = Fixture::new().await;
    let mut record = binding(/*observed_at_ms*/ 1_000_010);
    for reference in [
        "review-record",
        "review-record@0",
        "review-record@01",
        "review-record@+1",
        "review-record@4294967296",
        "review-record@1@2",
        "../private@1",
        "https://review@1",
        "review-record@1\n",
    ] {
        record.experiment_ref = Some(reference.to_string());
        assert!(matches!(
            fixture.store.record_work_binding(&record).await,
            Err(UsageStoreError::InvalidFact)
        ));
    }
    record.experiment_ref = Some("review-record@4294967295".to_string());
    record.workstream_id = Some("w".repeat(257));
    assert!(matches!(
        fixture.store.record_work_binding(&record).await,
        Err(UsageStoreError::InvalidFact)
    ));
    record.workstream_id = Some("spec".to_string());
    record.native_project_id = "project@1".to_string();
    assert!(matches!(
        fixture.store.record_work_binding(&record).await,
        Err(UsageStoreError::InvalidFact)
    ));
    record.native_project_id = "project-alpha".to_string();
    fixture
        .store
        .record_work_binding(&record)
        .await
        .expect("immutable review reference");
    fixture
        .begin(&fixture.operation(/*started_at_ms*/ 1_000_011))
        .await;
    let packet = fixture.packet().await;
    assert_eq!(
        packet.work_bindings.references[0].experiment_ref,
        record.experiment_ref
    );
    assert_eq!(packet.work_bindings.bound_operations, 1);
}

#[tokio::test]
async fn binding_fields_accept_256_bytes_without_relaxing_global_identifiers() {
    let fixture = Fixture::new().await;
    let mut record = binding(/*observed_at_ms*/ 1_000_010);
    record.native_project_id = "p".repeat(256);
    record.workstream_id = Some(format!("stream:{}", "w".repeat(249)));
    record.outcome_id = Some(format!("outcome:{}..", "o".repeat(246)));
    record.experiment_ref = Some(format!("review:{}..@4294967295", "e".repeat(236)));
    fixture
        .store
        .record_work_binding(&record)
        .await
        .expect("256-byte binding fields");
    fixture
        .begin(&fixture.operation(/*started_at_ms*/ 1_000_011))
        .await;
    let packet = fixture.packet().await;
    let reference = &packet.work_bindings.references[0];
    assert_eq!(
        (
            &reference.native_project_id,
            &reference.workstream_id,
            &reference.outcome_id,
            &reference.experiment_ref
        ),
        (
            &record.native_project_id,
            &record.workstream_id,
            &record.outcome_id,
            &record.experiment_ref
        )
    );
    assert!(ThreadId::new("t".repeat(129)).is_err());
    assert!(ThreadId::new("thread:child").is_err());
    for invalid in [
        NewWorkBinding {
            native_project_id: "p".repeat(257),
            ..record.clone()
        },
        NewWorkBinding {
            workstream_id: Some("w".repeat(257)),
            ..record.clone()
        },
        NewWorkBinding {
            outcome_id: Some("o".repeat(257)),
            ..record.clone()
        },
        NewWorkBinding {
            experiment_ref: Some(format!("{}@1", "e".repeat(255))),
            ..record.clone()
        },
    ] {
        assert!(matches!(
            fixture.store.record_work_binding(&invalid).await,
            Err(UsageStoreError::InvalidFact)
        ));
    }
    record.experiment_ref = Some(format!("{}@1", "e".repeat(254)));
    record.observed_at_ms = 1_000_020;
    fixture
        .store
        .record_work_binding(&record)
        .await
        .expect("full envelope with short revision");
    fixture
        .begin(&fixture.operation(/*started_at_ms*/ 1_000_021))
        .await;
    assert_eq!(
        fixture.packet().await.work_bindings.references[0].experiment_ref,
        record.experiment_ref
    );
}

#[tokio::test]
async fn populated_v5_upgrade_preserves_operations_without_backfilling_work_bindings() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let usage_dir = directory.path().join("usage");
    codex_private_storage::ensure_private_directory(&usage_dir).expect("usage directory");
    let database = usage_dir.join("usage.sqlite3");
    let pool =
        codex_state::open_sqlite_pool(&database, codex_state::SqlitePoolProfile::DurableEvents)
            .await
            .expect("v5 database");
    let mut connection = pool.acquire().await.expect("migration connection");
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await
        .expect("migration table");
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations.iter().take(5) {
        connection
            .apply("_sqlx_migrations", migration)
            .await
            .expect("v5 migration");
    }
    drop(connection);
    let operation_id = OperationId::new().as_string();
    sqlx::query("INSERT INTO process_instances(id, os_pid, started_at_ms) VALUES ('old-process', 1, 1000000)").execute(&pool).await.expect("old process");
    sqlx::query("INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms) VALUES ('spec', NULL, 'test', 1000000)").execute(&pool).await.expect("old thread");
    sqlx::query("INSERT INTO operations(id, process_id, thread_id, operation_kind, started_at_ms, taxonomy_version,
        phase, activity, activity_state, attribution_provenance) VALUES (?, 'old-process', 'spec', 'model_request',
        1000000, 1, 'planning', 'specification', 'model_active', 'unknown')").bind(&operation_id).execute(&pool).await.expect("old operation");
    pool.close().await;
    for path in [
        &database,
        &usage_dir.join("usage.sqlite3-wal"),
        &usage_dir.join("usage.sqlite3-shm"),
    ] {
        if path.exists() {
            codex_private_storage::ensure_private_file(path).expect("private fixture file");
        }
    }
    let store = UsageStore::open(directory.path())
        .await
        .expect("upgrade to v6");
    let operations: Vec<(String, i64)> = sqlx::query_as("SELECT id, started_at_ms FROM operations")
        .fetch_all(&store.pool)
        .await
        .expect("preserved operations");
    assert_eq!(operations, vec![(operation_id, 1_000_000)]);
    let bindings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_bindings")
        .fetch_one(&store.pool)
        .await
        .expect("empty additive fact table");
    assert_eq!(bindings, 0);
    store
        .record_work_binding(&binding(/*observed_at_ms*/ 1_000_010))
        .await
        .expect("new activation");
    let packet = store
        .performance_review_packet(PerformanceReviewQuery::default())
        .await
        .expect("upgraded packet");
    assert_eq!(
        (
            packet.work_bindings.bound_operations,
            packet.work_bindings.unknown_operations
        ),
        (0, 1)
    );
    assert!(packet.work_bindings.references.is_empty());
}
