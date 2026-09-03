use super::*;
use pretty_assertions::assert_eq;

fn material(value: &str) -> RepositoryIdentityMaterial {
    RepositoryIdentityMaterial::new(value).expect("valid identity material")
}

fn canonical(value: &str) -> CanonicalRepositoryPath {
    CanonicalRepositoryPath::new(value).expect("canonical repository path")
}

fn label(value: &str) -> SafeRepositoryLabel {
    SafeRepositoryLabel::new(value).expect("valid repository label")
}

#[cfg(unix)]
#[tokio::test]
async fn key_is_private_raw_identity_is_not_persisted_and_history_blocks_rekey() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let raw_origin = "ssh://private-user@Example.COM/private/secret-repository.git";
    let identity = RepositoryIdentityInput::new(canonical("/private/workspace"))
        .with_git_common_dir(canonical("/private/workspace/.git"))
        .with_origin(material(raw_origin));
    let repository_id = store
        .resolve_repository(
            &identity,
            &label("secret-repository"),
            /*observed_at_ms*/ 1_000,
        )
        .await
        .expect("resolve repository");

    let key_path = temp.path().join("usage").join(REPOSITORY_KEY_FILENAME);
    assert_eq!(
        0o600,
        std::fs::metadata(&key_path)
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777
    );
    let stored = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, identity_source, safe_display_label FROM repositories",
    )
    .fetch_one(&store.pool)
    .await
    .expect("stored repository");
    assert_eq!(stored.0, repository_id.as_str());
    assert_eq!(stored.1, "origin");
    assert_eq!(stored.2, "secret-repository");
    assert!(!stored.0.contains("private"));
    assert!(!format!("{repository_id:?}").contains(raw_origin));
    store.close().await;

    std::fs::remove_file(&key_path).expect("remove fixture key");
    assert!(matches!(
        UsageStore::open(temp.path()).await,
        Err(UsageStoreError::RepositoryKeyMissing)
    ));
    std::fs::write(&key_path, b"bad").expect("write corrupt fixture key");
    assert!(matches!(
        UsageStore::open(temp.path()).await,
        Err(UsageStoreError::RepositoryKeyCorrupt)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn origin_and_common_dir_normalization_are_stable_across_worktrees() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let first = RepositoryIdentityInput::new(canonical("/checkout-one"))
        .with_git_common_dir(canonical("/git/repository/.git"))
        .with_origin(material("HTTPS://user@GitHub.COM/Org/Repo.git/"));
    let second = RepositoryIdentityInput::new(canonical("/checkout-two"))
        .with_git_common_dir(canonical("/different/common-dir"))
        .with_origin(material("https://github.com/Org/Repo"));
    let first_id = store
        .resolve_repository(&first, &label("Repo"), /*observed_at_ms*/ 1_000)
        .await
        .expect("resolve first worktree");
    let second_id = store
        .resolve_repository(
            &second,
            &label("Other checkout"),
            /*observed_at_ms*/ 1_001,
        )
        .await
        .expect("resolve second worktree");
    assert_eq!(first_id, second_id);

    let common_one = RepositoryIdentityInput::new(canonical("/checkout-three"))
        .with_git_common_dir(canonical("/git/repository/.git"));
    let common_two = RepositoryIdentityInput::new(canonical("/checkout-four"))
        .with_git_common_dir(canonical("/git/repository/.git"));
    assert_eq!(
        store
            .resolve_repository(&common_one, &label("Common"), /*observed_at_ms*/ 1_002)
            .await
            .expect("resolve common one"),
        store
            .resolve_repository(&common_two, &label("Common"), /*observed_at_ms*/ 1_003)
            .await
            .expect("resolve common two")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn verified_identity_candidates_find_history_without_merging_path_or_remote_changes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let workspace_only = RepositoryIdentityInput::new(canonical("/checkout"));
    let workspace_id = store
        .resolve_repository(
            &workspace_only,
            &label("Checkout"),
            /*observed_at_ms*/ 1_000,
        )
        .await
        .expect("resolve workspace identity");
    let complete_identity = RepositoryIdentityInput::new(canonical("/checkout"))
        .with_git_common_dir(canonical("/git/repository/.git"))
        .with_origin(material("https://example.test/org/repository.git"));

    assert_eq!(
        store
            .find_repository_for_identity(&complete_identity)
            .await
            .expect("find less authoritative history"),
        Some(workspace_id.clone())
    );

    let origin_id = store
        .resolve_repository(
            &complete_identity,
            &label("Checkout"),
            /*observed_at_ms*/ 1_002,
        )
        .await
        .expect("resolve authoritative identity");
    assert_ne!(origin_id, workspace_id);
    assert_eq!(
        store
            .find_repository_for_identity(&complete_identity)
            .await
            .expect("prefer stored origin identity"),
        Some(origin_id.clone())
    );
    assert_eq!(
        store
            .find_repository_for_identity(&workspace_only)
            .await
            .expect("find workspace history independently"),
        Some(workspace_id.clone())
    );
    assert_eq!(
        store
            .canonical_repository_id(&workspace_id)
            .await
            .expect("workspace identity remains independent"),
        workspace_id
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM repository_merge_events")
            .fetch_one(&store.pool)
            .await
            .expect("merge count"),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn alias_and_merge_evidence_is_append_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let source = store
        .resolve_repository(
            &RepositoryIdentityInput::new(canonical("/source")),
            &label("Source"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("source repository");
    let target = store
        .resolve_repository(
            &RepositoryIdentityInput::new(canonical("/target")),
            &label("Target"),
            /*observed_at_ms*/ 2,
        )
        .await
        .expect("target repository");
    let alias_event_id = FactEventId::new();
    store
        .append_repository_alias(
            alias_event_id,
            &source,
            &label("Legacy"),
            /*occurred_at_ms*/ 3,
        )
        .await
        .expect("append alias");
    store
        .append_repository_alias(
            alias_event_id,
            &source,
            &label("Legacy"),
            /*occurred_at_ms*/ 3,
        )
        .await
        .expect("replay alias");
    assert!(matches!(
        store
            .append_repository_alias(
                alias_event_id,
                &source,
                &label("Different"),
                /*occurred_at_ms*/ 3
            )
            .await,
        Err(UsageStoreError::FactConflict)
    ));
    let merge_event_id = FactEventId::new();
    store
        .append_repository_merge(merge_event_id, &source, &target, /*occurred_at_ms*/ 4)
        .await
        .expect("append merge");
    store
        .append_repository_merge(merge_event_id, &source, &target, /*occurred_at_ms*/ 4)
        .await
        .expect("replay merge");
    assert!(matches!(
        store
            .append_repository_merge(merge_event_id, &source, &target, /*occurred_at_ms*/ 5)
            .await,
        Err(UsageStoreError::FactConflict)
    ));
    assert!(matches!(
        store
            .append_repository_merge(
                FactEventId::new(),
                &target,
                &source,
                /*occurred_at_ms*/ 6
            )
            .await,
        Err(UsageStoreError::RepositoryMergeCycle)
    ));
    assert_eq!(
        store
            .canonical_repository_id(&source)
            .await
            .expect("canonical repository"),
        target
    );
    assert_eq!(
        store
            .repository_display_label(&target)
            .await
            .expect("merged alias"),
        "Legacy"
    );
    assert_eq!(
        (1, 1),
        sqlx::query_as::<_, (i64, i64)>(
            r#"
            SELECT
                (SELECT COUNT(*) FROM repository_alias_events),
                (SELECT COUNT(*) FROM repository_merge_events)
            "#,
        )
        .fetch_one(&store.pool)
        .await
        .expect("repository evidence counts")
    );
    assert!(
        sqlx::query("DELETE FROM repository_merge_events")
            .execute(&store.pool)
            .await
            .is_err()
    );
}

#[test]
fn canonical_paths_reject_relative_dot_url_and_separator_aliases() {
    for invalid in [
        "relative/repo",
        "/repo/../other",
        "/repo/./worktree",
        "/repo//worktree",
        "/repo/worktree/",
        "https://example.com/repo",
        "\\repo\\worktree",
    ] {
        assert!(matches!(
            CanonicalRepositoryPath::new(invalid),
            Err(RepositoryIdentityError::InvalidCanonicalPath)
        ));
    }
}

#[test]
fn canonical_paths_accept_and_normalize_windows_drive_and_unc_shapes() {
    let drive_backslash = canonical(r"C:\Work\Repo");
    let drive_slash = canonical("c:/Work/Repo");
    let unc_backslash = canonical(r"\\Server\Share\Repo");
    let unc_slash = canonical("//Server/Share/Repo");

    assert_eq!(drive_backslash.0, drive_slash.0);
    assert_eq!(unc_backslash.0, unc_slash.0);
    for invalid in [
        r"C:relative\repo",
        r"C:\Work\..\Repo",
        r"\\Server\Share\Repo\",
        r"\\Server\Share\Repo\.\child",
        r"\\Server",
    ] {
        assert!(matches!(
            CanonicalRepositoryPath::new(invalid),
            Err(RepositoryIdentityError::InvalidCanonicalPath)
        ));
    }
}
