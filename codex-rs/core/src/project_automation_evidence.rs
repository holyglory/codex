use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ProjectAutomationCommand;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const MAX_OUTCOME_REFERENCES: usize = 32;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 15);
const TIMEOUT_ERROR: &str =
    "Coordinator evidence verification is incomplete: the whole query timed out";

pub async fn validate_project_evidence(
    command: &ProjectAutomationCommand,
    cwd: &Path,
    expected: Option<&ProjectAutomation>,
) -> Result<(), String> {
    validate_with_reader(command, cwd, expected, EVIDENCE_TIMEOUT, || {
        Command::new("devcoordinator2")
    })
    .await
}

async fn validate_with_reader(
    command: &ProjectAutomationCommand,
    cwd: &Path,
    expected: Option<&ProjectAutomation>,
    budget: Duration,
    reader: impl Fn() -> Command + Sync,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    let operation = async {
        let (category, method, reference) = match command {
            ProjectAutomationCommand::RecordDelivery { evidence_ref, .. } => {
                ("release", "evidence", evidence_ref.as_str())
            }
            ProjectAutomationCommand::CompleteReview { decision_ref, .. } => {
                ("review", "show", decision_ref.as_str())
            }
            ProjectAutomationCommand::Complete { outcome_ref } => {
                ("task", "history", outcome_ref.as_str())
            }
            ProjectAutomationCommand::Status
            | ProjectAutomationCommand::LinkWork { .. }
            | ProjectAutomationCommand::Bind { .. }
            | ProjectAutomationCommand::ActivateDelivery { .. }
            | ProjectAutomationCommand::Postpone { .. }
            | ProjectAutomationCommand::Pause { .. }
            | ProjectAutomationCommand::Resume { .. }
            | ProjectAutomationCommand::RequestReview { .. }
            | ProjectAutomationCommand::Transfer { .. } => return Ok(()),
        };
        validate_reference(reference)?;
        let mut siblings = BTreeSet::new();
        if let ProjectAutomationCommand::Complete { outcome_ref } = command {
            let project = expected.ok_or(
                "project completion verification is incomplete: missing current project identity",
            )?;
            let owner = project.thread_outcomes.get(&project.owner_thread_id.to_string())
                .ok_or("project completion verification is incomplete: the owner has no linked outcome")?;
            if owner != outcome_ref {
                return Err(
                    "project completion requires the owner's currently linked outcome".into(),
                );
            }
            for linked in project.thread_outcomes.values() {
                validate_reference(linked)?;
                if linked != owner {
                    siblings.insert(linked.as_str());
                    if siblings.len() >= MAX_OUTCOME_REFERENCES {
                        return Err("project completion verification is incomplete: more than 32 distinct linked outcomes".into());
                    }
                }
            }
        }
        let repository = coordinator_json(
            cwd,
            &["repository", "status", "--format", "json"],
            reader(),
            deadline,
        )
        .await?;
        let repository_id = repository
            .get("repository_id")
            .or_else(|| repository.pointer("/repository/repository_id"))
            .and_then(Value::as_str)
            .ok_or("Coordinator did not identify the current repository")?;
        for requested in std::iter::once(reference).chain(siblings) {
            let record = coordinator_json(
                cwd,
                &[category, method, requested, "--format", "json"],
                reader(),
                deadline,
            )
            .await?;
            validate_record(command, &record, repository_id, requested)?;
            if let ProjectAutomationCommand::CompleteReview { job_id, .. } = command {
                let project = expected.ok_or("missing project review identity")?;
                let job = project.review.as_ref().ok_or("no pending review job")?;
                if job.id != *job_id
                    || record
                        .pointer("/record/windowStartMs")
                        .and_then(Value::as_i64)
                        != Some(project.review_window_start_ms)
                    || record
                        .pointer("/record/windowEndMs")
                        .and_then(Value::as_i64)
                        != Some(job.due_at_ms)
                {
                    return Err("review receipt does not cover this pending review window".into());
                }
            }
        }
        Ok(())
    };
    tokio::time::timeout_at(deadline, operation)
        .await
        .map_err(|_| TIMEOUT_ERROR.to_owned())?
}

fn validate_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty()
        || reference.len() > 256
        || !reference
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || b"-_:/.@".contains(&value))
        || reference.starts_with('-')
    {
        return Err("invalid qualified evidence reference".into());
    }
    Ok(())
}

fn validate_record(
    command: &ProjectAutomationCommand,
    record: &Value,
    repository_id: &str,
    reference: &str,
) -> Result<(), String> {
    match command {
        ProjectAutomationCommand::RecordDelivery {
            target,
            delivered_at_ms,
            ..
        } => {
            let observed = record
                .get("delivered_at_ms")
                .and_then(Value::as_i64)
                .or_else(|| record.get("verified_at_ms").and_then(Value::as_i64));
            if record.get("qualified").and_then(Value::as_bool) != Some(true)
                || record.get("qualification").and_then(Value::as_str) != Some("qualified")
                || record.get("repository_id").and_then(Value::as_str) != Some(repository_id)
                || record.get("target").and_then(Value::as_str) != Some(target.as_str())
                || observed != Some(*delivered_at_ms)
                || record
                    .get("access")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                || record
                    .get("verification_sha256")
                    .and_then(Value::as_str)
                    .is_none_or(|digest| digest.len() != 64)
            {
                return Err("delivery requires a qualified, same-repository/target verification receipt and its actual observed delivery timestamp".into());
            }
        }
        ProjectAutomationCommand::CompleteReview { .. } => {
            if record.get("completed").and_then(Value::as_bool) != Some(true)
                || record
                    .pointer("/record/workstreamId")
                    .is_some_and(|scope| !scope.is_null())
                || record
                    .pointer("/record/repositoryId")
                    .and_then(Value::as_str)
                    != Some(repository_id)
                || record.pointer("/record/projectId").and_then(Value::as_str)
                    != Some(repository_id)
                || record
                    .pointer("/record/experiment/scopeRepoId")
                    .and_then(Value::as_str)
                    != Some(repository_id)
                || !matches!(
                    record
                        .pointer("/record/experiment/disposition")
                        .and_then(Value::as_str),
                    Some("retained" | "reverted" | "inconclusive" | "unchanged")
                )
            {
                return Err("review completion requires a completed project-wide structured decision for this repository, without a workstream filter".into());
            }
        }
        ProjectAutomationCommand::Complete { .. } => {
            if record.pointer("/task/task_id").and_then(Value::as_str) != Some(reference)
                || record.pointer("/task/status").and_then(Value::as_str) != Some("done")
                || record
                    .pointer("/task/repository_id")
                    .and_then(Value::as_str)
                    != Some(repository_id)
            {
                return Err("project completion verification is incomplete: a linked outcome is not the matching completed Coordinator task in this repository".into());
            }
        }
        ProjectAutomationCommand::Status
        | ProjectAutomationCommand::LinkWork { .. }
        | ProjectAutomationCommand::Bind { .. }
        | ProjectAutomationCommand::ActivateDelivery { .. }
        | ProjectAutomationCommand::Postpone { .. }
        | ProjectAutomationCommand::Pause { .. }
        | ProjectAutomationCommand::Resume { .. }
        | ProjectAutomationCommand::RequestReview { .. }
        | ProjectAutomationCommand::Transfer { .. } => {}
    }
    Ok(())
}

async fn coordinator_json(
    cwd: &Path,
    args: &[&str],
    mut reader: Command,
    deadline: tokio::time::Instant,
) -> Result<Value, String> {
    if tokio::time::Instant::now() >= deadline {
        return Err(TIMEOUT_ERROR.into());
    }
    let mut process = reader
        .args(args)
        .current_dir(cwd)
        .env_remove("DEVCOORDINATOR_WORK_CONTEXT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Coordinator evidence reader is unavailable".to_owned())?;
    let mut output = Vec::new();
    process
        .stdout
        .take()
        .ok_or("missing evidence output")?
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut output)
        .await
        .map_err(|_| "cannot read evidence")?;
    if output.len() > MAX_RESPONSE_BYTES {
        return Err("Coordinator evidence exceeds the bounded response size".into());
    }
    let status = process.wait().await.map_err(|_| "evidence reader failed")?;
    let value: Value =
        serde_json::from_slice(&output).map_err(|_| "invalid Coordinator evidence response")?;
    if !status.success() || value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("qualified Coordinator evidence is unavailable; preserve the existing deadline and retry after evidence is recorded".into());
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(TIMEOUT_ERROR.into());
    }
    value
        .get("data")
        .cloned()
        .ok_or_else(|| "missing evidence result".into())
}

#[cfg(test)]
#[path = "project_automation_evidence_tests.rs"]
mod tests;
