use super::*;
use std::path::PathBuf;

use codex_event_subscriptions::AutomationJob;
use codex_event_subscriptions::AutomationJobKind;
use codex_event_subscriptions::WorkPurpose;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

const START_MS: i64 = 1_000_000;

struct Getter {
    directory: TempDir,
    script: PathBuf,
}

impl Getter {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("private getter fixture");
        #[cfg(unix)]
        let (name, script) = (
            "getter.sh",
            r#"#!/bin/sh
if [ "${DEVCOORDINATOR_WORK_CONTEXT+x}" ]; then exit 31; fi
printf '%s %s %s\n' "$1" "$2" "${3-}" >> calls.log
if [ -f fail ]; then /bin/cat failure.json; exit 9; fi
case "$1:$2" in
  repository:status) /bin/cat repository.json ;;
  release:evidence) /bin/cat delivery.json ;;
  review:show) /bin/cat review.json ;;
  task:history) /bin/cat "task-$3.json" ;;
  *) exit 32 ;;
esac
"#,
        );
        #[cfg(windows)]
        let (name, script) = (
            "getter.cmd",
            r#"@echo off
if defined DEVCOORDINATOR_WORK_CONTEXT exit /b 31
echo %~1 %~2 %~3>>calls.log
if exist fail goto failure
if "%~1"=="repository" goto repository
if "%~1"=="release" goto delivery
if "%~1"=="review" goto review
if "%~1"=="task" goto task
exit /b 32
:repository
type repository.json
exit /b
:delivery
type delivery.json
exit /b
:review
type review.json
exit /b
:task
type task-%~3.json
exit /b
:failure
type failure.json
exit /b 9
"#,
        );
        let path = directory.path().join(name);
        std::fs::write(&path, script).expect("getter script");
        let fixture = Self {
            directory,
            script: path,
        };
        fixture.reply("repository.json", json!({"repository_id":"repo"}));
        fixture
    }

    fn reply(&self, file: &str, data: Value) {
        self.raw(file, &json!({"ok":true,"data":data}).to_string());
    }

    fn raw(&self, file: &str, contents: &str) {
        std::fs::write(self.directory.path().join(file), contents).expect("getter response");
    }

    fn task(&self, reference: &str, status: &str, repository: &str) {
        self.reply(
            &format!("task-{reference}.json"),
            json!({"task":{"task_id":reference,"status":status,"repository_id":repository}}),
        );
    }

    fn command(&self) -> Command {
        #[cfg(unix)]
        let mut command = Command::new("/bin/sh");
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/Q", "/C"]);
            command
        };
        command
            .arg(&self.script)
            .env("DEVCOORDINATOR_WORK_CONTEXT", "must-not-reach-getter");
        command
    }

    async fn validate(
        &self,
        command: &ProjectAutomationCommand,
        expected: Option<&ProjectAutomation>,
    ) -> Result<(), String> {
        validate_with_reader(
            command,
            self.directory.path(),
            expected,
            EVIDENCE_TIMEOUT,
            || self.command(),
        )
        .await
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.directory.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .map(str::to_owned)
            .collect()
    }
}

fn project() -> ProjectAutomation {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("native-clock-project".into(), owner, START_MS);
    project
        .threads
        .insert(owner.to_string(), WorkPurpose::Implementation);
    project
        .thread_outcomes
        .insert(owner.to_string(), "owner".into());
    project
}

fn link(project: &mut ProjectAutomation, reference: &str) {
    let thread = ThreadId::new().to_string();
    project
        .threads
        .insert(thread.clone(), WorkPurpose::Implementation);
    project.thread_outcomes.insert(thread, reference.into());
}

fn delivery() -> Value {
    json!({"qualified":true,"qualification":"qualified","repository_id":"repo","target":"linux","delivered_at_ms":100,"verified_at_ms":100,"access":"https://example.invalid/download","verification_sha256":"a".repeat(64)})
}

#[tokio::test]
async fn getter_delivery_accepts_verified_data_and_rejects_mismatches() {
    let fixture = Getter::new();
    let command = ProjectAutomationCommand::RecordDelivery {
        target: "linux".into(),
        delivered_at_ms: 100,
        evidence_ref: "delivery-test".into(),
    };
    let valid = delivery();
    fixture.reply("delivery.json", valid.clone());
    assert_eq!(fixture.validate(&command, /*expected*/ None).await, Ok(()));
    for (field, value) in [
        ("qualified", json!(false)),
        ("target", json!("windows")),
        ("repository_id", json!("other")),
        ("delivered_at_ms", json!(101)),
        ("verification_sha256", json!(null)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        fixture.reply("delivery.json", invalid);
        assert!(fixture.validate(&command, /*expected*/ None).await.is_err());
    }
}

#[tokio::test]
async fn getter_review_requires_current_project_wide_window() {
    let fixture = Getter::new();
    let mut project = project();
    let job_id = Uuid::now_v7();
    let due_at_ms = START_MS + 1_000;
    project.review = Some(AutomationJob {
        id: job_id,
        kind: AutomationJobKind::PerformanceReview,
        due_at_ms,
        revision: project.revision,
        notified: true,
        decision_ref: None,
    });
    let command = ProjectAutomationCommand::CompleteReview {
        job_id,
        decision_ref: "review@1".into(),
    };
    let valid = json!({"completed":true,"record":{"repositoryId":"repo","projectId":"repo","workstreamId":null,"windowStartMs":START_MS,"windowEndMs":due_at_ms,"experiment":{"scopeRepoId":"repo","disposition":"unchanged"}}});
    fixture.reply("review.json", valid.clone());
    assert_eq!(fixture.validate(&command, Some(&project)).await, Ok(()));
    let mut absent_scope = valid.clone();
    absent_scope["record"]
        .as_object_mut()
        .unwrap()
        .remove("workstreamId");
    fixture.reply("review.json", absent_scope);
    assert_eq!(fixture.validate(&command, Some(&project)).await, Ok(()));
    for (field, value) in [
        ("windowStartMs", json!(START_MS - 1)),
        ("windowEndMs", json!(due_at_ms + 1)),
        ("repositoryId", json!("foreign")),
        ("workstreamId", json!("implementation")),
        ("workstreamId", json!("")),
    ] {
        let mut invalid = valid.clone();
        invalid["record"][field] = value;
        fixture.reply("review.json", invalid);
        assert!(fixture.validate(&command, Some(&project)).await.is_err());
    }
    fixture.reply("review.json", valid);
    let stale_job = ProjectAutomationCommand::CompleteReview {
        job_id: Uuid::now_v7(),
        decision_ref: "review@1".into(),
    };
    assert!(fixture.validate(&stale_job, Some(&project)).await.is_err());
}

#[tokio::test]
async fn getter_completion_checks_distinct_current_links_without_unrelated_outcomes() {
    let fixture = Getter::new();
    let mut project = project();
    link(&mut project, "sibling");
    link(&mut project, "sibling");
    link(&mut project, "owner");
    fixture.task("owner", "done", "repo");
    fixture.task("sibling", "done", "repo");
    fixture.task("unrelated", "in_progress", "repo");
    let command = ProjectAutomationCommand::Complete {
        outcome_ref: "owner".into(),
    };
    assert_eq!(fixture.validate(&command, Some(&project)).await, Ok(()));
    assert_eq!(
        fixture.calls(),
        vec![
            "repository status --format",
            "task history owner",
            "task history sibling"
        ]
    );
}

#[tokio::test]
async fn getter_completion_rejects_unrelated_done_task_and_unbound_owner() {
    let fixture = Getter::new();
    let mut project = project();
    fixture.task("owner", "done", "repo");
    fixture.task("unrelated", "done", "repo");
    let unrelated = ProjectAutomationCommand::Complete {
        outcome_ref: "unrelated".into(),
    };
    assert!(fixture.validate(&unrelated, Some(&project)).await.is_err());
    project.thread_outcomes.clear();
    let owner = ProjectAutomationCommand::Complete {
        outcome_ref: "owner".into(),
    };
    assert!(
        fixture
            .validate(&owner, Some(&project))
            .await
            .unwrap_err()
            .contains("incomplete")
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn getter_completion_rejects_unfinished_foreign_and_wrong_identity_siblings() {
    let fixture = Getter::new();
    let mut project = project();
    link(&mut project, "sibling");
    fixture.task("owner", "done", "repo");
    let command = ProjectAutomationCommand::Complete {
        outcome_ref: "owner".into(),
    };
    for (task_id, status, repository) in [
        ("sibling", "in_progress", "repo"),
        ("sibling", "done", "foreign"),
        ("different", "done", "repo"),
    ] {
        fixture.reply(
            "task-sibling.json",
            json!({"task":{"task_id":task_id,"status":status,"repository_id":repository}}),
        );
        assert!(
            fixture
                .validate(&command, Some(&project))
                .await
                .unwrap_err()
                .contains("incomplete")
        );
    }
    fixture.reply(
        "task-owner.json",
        json!({"task":{"task_id":"different","status":"done","repository_id":"repo"}}),
    );
    assert!(fixture.validate(&command, Some(&project)).await.is_err());
}

#[tokio::test]
async fn getter_completion_limits_distinct_references_without_truncation() {
    let fixture = Getter::new();
    let mut project = project();
    fixture.task("owner", "done", "repo");
    for index in 0..MAX_OUTCOME_REFERENCES - 1 {
        let reference = format!("sibling-{index}");
        link(&mut project, &reference);
        fixture.task(&reference, "done", "repo");
    }
    let command = ProjectAutomationCommand::Complete {
        outcome_ref: "owner".into(),
    };
    assert_eq!(fixture.validate(&command, Some(&project)).await, Ok(()));
    assert_eq!(fixture.calls().len(), MAX_OUTCOME_REFERENCES + 1);
    link(&mut project, "overflow");
    assert!(
        fixture
            .validate(&command, Some(&project))
            .await
            .unwrap_err()
            .contains("incomplete")
    );
    assert_eq!(fixture.calls().len(), MAX_OUTCOME_REFERENCES + 1);
}

#[tokio::test]
async fn getter_pipeline_retains_json_success_and_size_validation() {
    let fixture = Getter::new();
    let command = ProjectAutomationCommand::RecordDelivery {
        target: "linux".into(),
        delivered_at_ms: 100,
        evidence_ref: "delivery-test".into(),
    };
    fixture.raw("delivery.json", "not JSON");
    assert!(
        fixture
            .validate(&command, /*expected*/ None)
            .await
            .unwrap_err()
            .contains("invalid Coordinator evidence")
    );
    fixture.raw("delivery.json", r#"{"ok":false,"data":{}}"#);
    assert!(fixture.validate(&command, /*expected*/ None).await.is_err());
    fixture.reply(
        "delivery.json",
        json!({"padding":"x".repeat(MAX_RESPONSE_BYTES)}),
    );
    assert!(
        fixture
            .validate(&command, /*expected*/ None)
            .await
            .unwrap_err()
            .contains("bounded response size")
    );
    fixture.reply("failure.json", delivery());
    fixture.raw("fail", "");
    assert!(
        fixture
            .validate(&command, /*expected*/ None)
            .await
            .unwrap_err()
            .contains("evidence is unavailable")
    );
}

#[tokio::test]
async fn getter_completion_has_one_budget_for_the_entire_query() {
    let fixture = Getter::new();
    let mut project = project();
    link(&mut project, "sibling");
    for reference in ["owner", "sibling"] {
        fixture.task(reference, "done", "repo");
    }
    let command = ProjectAutomationCommand::Complete {
        outcome_ref: "owner".into(),
    };
    let result = validate_with_reader(
        &command,
        fixture.directory.path(),
        Some(&project),
        Duration::from_secs(/*secs*/ 1),
        || {
            std::thread::sleep(Duration::from_millis(/*millis*/ 400));
            fixture.command()
        },
    )
    .await;
    assert_eq!(result, Err(TIMEOUT_ERROR.into()));
    assert!(
        !fixture
            .calls()
            .iter()
            .any(|call| call == "task history sibling")
    );
}
