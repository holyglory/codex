use anyhow::Result;
use codex_usage::Activity;
use codex_usage::ActivityState;
use codex_usage::AttributionProvenance;
use codex_usage::CanonicalRepositoryPath;
use codex_usage::FactEventId;
use codex_usage::NewOperation;
use codex_usage::NewRepositoryAttribution;
use codex_usage::NewThread;
use codex_usage::OperationId;
use codex_usage::OperationKind;
use codex_usage::Phase;
use codex_usage::ProcessId;
use codex_usage::RepositoryAttributionKind;
use codex_usage::RepositoryAttributionProvenance;
use codex_usage::RepositoryIdentityInput;
use codex_usage::SafeRepositoryLabel;
use codex_usage::TerminalOperation;
use codex_usage::TerminalStatus;
use codex_usage::ThreadId;
use codex_usage::ThreadSourceKind;
use codex_usage::UsageStore;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home)
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("OPENAI_API_KEY");
    Ok(command)
}

fn stdout_json(assertion: assert_cmd::assert::Assert) -> Result<Value> {
    Ok(serde_json::from_slice(&assertion.get_output().stdout)?)
}

#[tokio::test]
async fn usage_details_and_current_repository_run_through_the_codex_binary() -> Result<()> {
    const OS_PID_SENTINEL: u32 = 4_294_000_001;
    let home = TempDir::new()?;
    let checkout = TempDir::new()?;
    std::fs::create_dir(checkout.path().join(".git"))?;
    let store = UsageStore::open(home.path()).await?;
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, OS_PID_SENTINEL, /*started_at_ms*/ 1)
        .await?;
    let thread_id = ThreadId::new("thread-cli")?;
    store
        .ensure_thread(&NewThread {
            id: thread_id.clone(),
            parent_thread_id: None,
            source_kind: ThreadSourceKind::new("cli")?,
            created_at_ms: 2,
        })
        .await?;
    let operation_id = OperationId::new();
    store
        .begin_operation(&NewOperation {
            id: operation_id,
            process_id,
            thread_id: Some(thread_id),
            turn_id: None,
            agent_id: None,
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            kind: OperationKind::LocalTool,
            started_at_ms: 3,
            phase: Phase::Testing,
            activity: Activity::VerificationReview,
            activity_state: ActivityState::ToolActive,
            attribution_provenance: AttributionProvenance::DeterministicClassification,
        })
        .await?;
    store
        .finish_operation(&TerminalOperation {
            operation_id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 4,
            duration_ns: 1,
            error_category: None,
        })
        .await?;
    let workspace = std::fs::canonicalize(checkout.path())?;
    let repository = store
        .resolve_repository(
            &RepositoryIdentityInput::new(CanonicalRepositoryPath::new(
                workspace.to_string_lossy(),
            )?),
            &SafeRepositoryLabel::new("checkout")?,
            /*observed_at_ms*/ 5,
        )
        .await?;
    store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: FactEventId::new(),
            operation_id,
            repository_id: Some(repository.clone()),
            kind: RepositoryAttributionKind::Primary,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms: 6,
        })
        .await?;

    let details = stdout_json(
        codex_command(home.path())?
            .args(["usage", "--json", "details", "operations"])
            .assert()
            .success(),
    )?;
    assert_eq!(details["schemaVersion"], 1);
    assert_eq!(details["kind"], "usageDetails");
    assert_eq!(details["detailKind"], "operations");
    assert_eq!(details["data"][0]["id"], operation_id.as_string());
    assert_eq!(details["data"][0]["type"], "operation");
    let encoded = details.to_string();
    assert!(!encoded.contains("osPid"));
    assert!(!encoded.contains(&OS_PID_SENTINEL.to_string()));

    let repository_summary = stdout_json(
        codex_command(home.path())?
            .current_dir(checkout.path())
            .args(["usage", "--json", "repo", "current"])
            .assert()
            .success(),
    )?;
    assert_eq!(repository_summary["schemaVersion"], 1);
    assert_eq!(repository_summary["scope"]["type"], "repository");
    assert_eq!(repository_summary["scope"]["id"], repository.as_str());
    Ok(())
}
