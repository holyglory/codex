use super::*;
use codex_usage::UsagePageCursor;
use pretty_assertions::assert_eq;

#[test]
fn parser_exposes_functional_surface_without_untruthful_current() {
    for args in [
        vec!["usage", "summary"],
        vec!["usage", "chat", "thread-1"],
        vec!["usage", "repo"],
        vec!["usage", "repo", "alias", "repo", "alias"],
        vec!["usage", "repo", "merge", "source", "target"],
        vec!["usage", "repositories"],
        vec!["usage", "tools"],
        vec!["usage", "activities"],
        vec!["usage", "events"],
        vec!["usage", "details", "operations"],
        vec!["usage", "details", "operations", "--repo", "current"],
        vec![
            "usage",
            "classify",
            "00000000-0000-4000-8000-000000000000",
            "--phase",
            "implementation",
            "--activity",
            "coding",
        ],
        vec![
            "usage",
            "export",
            "--output",
            "usage.jsonl",
            "--format",
            "jsonl",
        ],
        vec!["usage", "doctor"],
    ] {
        UsageCommand::try_parse_from(args).expect("command should parse");
    }
    assert!(UsageCommand::try_parse_from(["usage", "current"]).is_err());
    for kind in UsageDetailKind::ALL {
        UsageCommand::try_parse_from(["usage", "details", kind.as_str()])
            .expect("detail command should parse");
    }
}

#[test]
fn unsupported_filters_and_breakdowns_are_rejected() {
    let filters = UsageFilters {
        model: Some("model".to_string()),
        ..UsageFilters::default()
    };
    assert!(filters.ensure_only(&["thread"], &[]).is_err());
    let filters = UsageFilters {
        breakdown: vec!["model".to_string()],
        ..UsageFilters::default()
    };
    assert!(filters.ensure_only(&[], &["activity"]).is_err());
}

#[test]
fn opaque_cursors_are_kind_bound_and_round_trip() {
    let cursor = UsagePageCursor::new(/*occurred_at_ms*/ 123, "safe-id").expect("cursor");
    let filters = UsageFilters::default();
    let kind = filters.cursor_kind("tools");
    let encoded = encode_cursor(&kind, &cursor);
    assert_eq!(
        decode_cursor(&kind, &encoded).expect("decode cursor"),
        cursor
    );
    assert!(decode_cursor(&filters.cursor_kind("activities"), &encoded).is_err());
    assert!(decode_cursor(&kind, "not-hex").is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn empty_json_summary_preserves_unknowns_and_formulas() {
    let home = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(home.path()).await.expect("store");
    let summary = store
        .usage_summary_query(UsageSummaryQuery {
            thread_id: None,
            repository_id: None,
            account_profile_ref: None,
            time_range: None,
        })
        .await
        .expect("summary");
    let output = render::summary(&summary, /*json_output*/ true, Some("primary")).expect("render");
    let value: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["databaseSchemaVersion"], 5);
    assert_eq!(value["taxonomyVersion"], 1);
    assert_eq!(value["account"], "primary");
    assert_eq!(value["coverage"]["state"], "unobserved");
    assert!(value["formulas"]["tokens"].is_string());
    assert_eq!(value["providerTokens"], serde_json::json!([]));
}

#[cfg(unix)]
#[tokio::test]
async fn current_repository_matches_history_captured_with_less_identity_metadata() {
    let home = tempfile::tempdir().expect("home");
    let checkout = tempfile::tempdir().expect("checkout");
    std::fs::create_dir(checkout.path().join(".git")).expect("git directory");
    let workspace = std::fs::canonicalize(checkout.path()).expect("canonical checkout");
    let identity = RepositoryIdentityInput::new(
        CanonicalRepositoryPath::new(workspace.to_string_lossy()).expect("workspace identity"),
    );
    let store = UsageStore::open(home.path()).await.expect("store");
    let expected = store
        .resolve_repository(
            &identity,
            &SafeRepositoryLabel::new("checkout").expect("label"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("captured workspace identity");

    assert_eq!(
        resolve_current_repository(&store, checkout.path())
            .await
            .expect("current repository"),
        expected
    );
}

#[cfg(unix)]
#[tokio::test]
async fn export_is_private_and_never_clobbers() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(home.path()).await.expect("store");
    let output = home.path().join("usage.jsonl");
    let query = UsageEventListQuery {
        page: UsagePageRequest {
            cursor: None,
            limit: 10,
        },
        time_range: None,
        thread_id: None,
        repository_id: None,
        kind: None,
    };
    assert_eq!(
        export::write(
            &store,
            &output,
            ExportFormat::Jsonl,
            query.clone(),
            /*provenance*/ None,
            /*coverage*/ None,
        )
        .await
        .expect("export"),
        0
    );
    assert_eq!(
        std::fs::metadata(&output)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        export::write(
            &store,
            &output,
            ExportFormat::Jsonl,
            query,
            /*provenance*/ None,
            /*coverage*/ None,
        )
        .await
        .is_err()
    );
}
