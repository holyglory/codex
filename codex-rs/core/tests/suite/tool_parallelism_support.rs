use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::ToolStartInput;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::TestTargetOs;
use core_test_support::responses::ResponsesRequest;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::test_target_os;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::Barrier;
use tokio::sync::oneshot;
use uuid::Uuid;

const PARALLELISM_GUARD_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);
const HANDLER_COORDINATION_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);
const EXEC_COMMAND_YIELD_TIME_MS: u64 = 15_000;
pub(super) const MIXED_PARALLEL_TOOL_NAME: &str = "parallel_test_tool";

type ToolLifecycleRecord = (String, String, ToolCallOutcome);

struct ParallelToolStartBarrier {
    barrier: Barrier,
    timed_out: AtomicBool,
    finishes: Mutex<Vec<ToolLifecycleRecord>>,
}

impl ParallelToolStartBarrier {
    fn new() -> Self {
        Self {
            barrier: Barrier::new(/*n*/ 2),
            timed_out: AtomicBool::default(),
            finishes: Mutex::default(),
        }
    }

    fn evidence(&self) -> (bool, Vec<ToolLifecycleRecord>) {
        let mut finishes = self
            .finishes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        finishes.sort_by(|left, right| left.0.cmp(&right.0));
        (self.timed_out.load(Ordering::Acquire), finishes)
    }
}

impl ToolLifecycleContributor for ParallelToolStartBarrier {
    fn on_tool_start<'a>(&'a self, _input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if tokio::time::timeout(PARALLELISM_GUARD_TIMEOUT, self.barrier.wait())
                .await
                .is_err()
            {
                self.timed_out.store(true, Ordering::Release);
            }
        })
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        let record = (
            input.call_id.to_string(),
            input.tool_name.to_string(),
            input.outcome,
        );
        Box::pin(async move {
            self.finishes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record);
        })
    }
}

struct ParallelReleaseTool {
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

struct ParallelReleaseToolContributor {
    tool: Arc<ParallelReleaseTool>,
}

impl ToolContributor for ParallelReleaseToolContributor {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        vec![self.tool.clone()]
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for ParallelReleaseTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(MIXED_PARALLEL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: MIXED_PARALLEL_TOOL_NAME.to_string(),
            description: "Test-only handler released after exec_command starts.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: Default::default(),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, _call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        let started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let release = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Box::pin(async move {
            let started = started.ok_or_else(|| {
                FunctionCallError::Fatal("parallel test tool started more than once".to_string())
            })?;
            started.send(()).map_err(|()| {
                FunctionCallError::Fatal(
                    "parallel test tool start receiver was dropped".to_string(),
                )
            })?;
            let release = release.ok_or_else(|| {
                FunctionCallError::Fatal("parallel test tool has no release receiver".to_string())
            })?;
            tokio::time::timeout(PARALLELISM_GUARD_TIMEOUT, release)
                .await
                .map_err(|_| {
                    FunctionCallError::timed_out("parallel test tool timed out waiting for release")
                })?
                .map_err(|_| {
                    FunctionCallError::Fatal(
                        "parallel test tool release sender was dropped".to_string(),
                    )
                })?;
            Ok(Box::new(JsonToolOutput::new(json!({
                "tool": MIXED_PARALLEL_TOOL_NAME
            }))) as Box<dyn ToolOutput>)
        })
    }
}

pub(super) struct ParallelFixture {
    test: TestCodex,
    barrier: Arc<ParallelToolStartBarrier>,
}

impl ParallelFixture {
    pub(super) async fn run_turn(&self, prompt: &str) -> anyhow::Result<String> {
        run_turn(&self.test, prompt).await
    }

    pub(super) fn assert_lifecycle(&self, expected_tools: &[(&str, &str)]) {
        assert_parallel_tool_lifecycle(&self.barrier, expected_tools);
    }

    pub(super) fn shell_exec_arguments(&self) -> [String; 2] {
        let run_id = Uuid::new_v4();
        let first = format!("parallel-shell-first-{run_id}.started");
        let second = format!("parallel-shell-second-{run_id}.started");
        [
            marker_wait_exec_arguments(&first, &second),
            marker_wait_exec_arguments(&second, &first),
        ]
    }
}

pub(super) struct MixedParallelFixture {
    pub(super) parallel: ParallelFixture,
    custom_started: Option<oneshot::Receiver<()>>,
    release_custom: Option<oneshot::Sender<()>>,
    exec_marker: String,
}

impl MixedParallelFixture {
    pub(super) fn exec_arguments(&self) -> String {
        mixed_exec_arguments(&self.exec_marker)
    }

    pub(super) async fn run_turn(&mut self, prompt: &str) -> anyhow::Result<String> {
        let custom_started = self
            .custom_started
            .take()
            .context("mixed parallel fixture should run once")?;
        let release_custom = self
            .release_custom
            .take()
            .context("mixed parallel fixture should release once")?;
        let turn_id = submit_turn(&self.parallel.test, prompt).await?;
        let turn_completed = Arc::new(AtomicBool::default());
        let proof = tokio::time::timeout(HANDLER_COORDINATION_TIMEOUT, async {
            let custom_started = async {
                custom_started
                    .await
                    .context("parallel test tool should signal handler start")
            };
            let exec_started = wait_for_exec_start(
                &self.parallel.test,
                &turn_id,
                &self.exec_marker,
                Arc::clone(&turn_completed),
            );
            tokio::try_join!(custom_started, exec_started)?;
            Ok::<(), anyhow::Error>(())
        })
        .await;
        let proof_result = match proof {
            Ok(result) => result,
            Err(err) => Err(anyhow::Error::new(err)
                .context("mixed parallel handlers should both start before release")),
        };

        let release_result = release_custom.send(());
        if !turn_completed.load(Ordering::Acquire) {
            wait_for_event(&self.parallel.test.codex, |event| match event {
                EventMsg::TurnComplete(event) => event.turn_id == turn_id,
                _ => false,
            })
            .await;
        }

        proof_result?;
        release_result.map_err(|()| {
            anyhow::anyhow!("parallel test tool should still be waiting for release")
        })?;
        Ok(turn_id)
    }
}

fn parallel_start_extension_builder() -> (
    ExtensionRegistryBuilder<Config>,
    Arc<ParallelToolStartBarrier>,
) {
    let barrier = Arc::new(ParallelToolStartBarrier::new());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(barrier.clone());
    (extensions, barrier)
}

fn assert_parallel_tool_lifecycle(
    barrier: &ParallelToolStartBarrier,
    expected_tools: &[(&str, &str)],
) {
    let finishes = expected_tools
        .iter()
        .map(|(call_id, tool_name)| {
            (
                (*call_id).to_string(),
                (*tool_name).to_string(),
                ToolCallOutcome::Completed { success: true },
            )
        })
        .collect();
    assert_eq!(barrier.evidence(), (false, finishes));
}

fn marker_wait_exec_arguments(marker: &str, peer: &str) -> String {
    let guard_seconds = PARALLELISM_GUARD_TIMEOUT.as_secs();
    let (shell, cmd) = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => (
            "bash",
            format!(
                r#"set -eu
: > '{marker}'
deadline=$((SECONDS + {guard_seconds}))
while [ ! -f '{peer}' ]; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        echo 'timed out waiting for {peer}' >&2
        exit 1
    fi
    sleep 0.05
done"#,
            ),
        ),
        TestTargetOs::Windows => (
            "powershell",
            format!(
                r#"$ErrorActionPreference = 'Stop'
Set-Content -LiteralPath '{marker}' -Value 'started'
$deadline = (Get-Date).AddSeconds({guard_seconds})
while (-not (Test-Path -LiteralPath '{peer}')) {{
    if ((Get-Date) -ge $deadline) {{
        [Console]::Error.WriteLine('timed out waiting for {peer}')
        exit 1
    }}
    Start-Sleep -Milliseconds 50
}}"#,
            ),
        ),
    };
    json!({
        "cmd": cmd,
        "shell": shell,
        "login": false,
        "yield_time_ms": EXEC_COMMAND_YIELD_TIME_MS,
    })
    .to_string()
}

fn mixed_exec_arguments(marker: &str) -> String {
    let (shell, cmd) = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => (
            "bash",
            format!(
                r#"printf '%s\n' '{marker}'
sleep 8"#,
            ),
        ),
        TestTargetOs::Windows => (
            "powershell",
            format!(
                r#"Write-Output '{marker}'
Start-Sleep -Seconds 8"#,
            ),
        ),
    };
    json!({
        "cmd": cmd,
        "shell": shell,
        "login": false,
        "yield_time_ms": EXEC_COMMAND_YIELD_TIME_MS,
    })
    .to_string()
}

pub(super) fn assert_exec_command_succeeded(request: &ResponsesRequest, call_id: &str) {
    let output = request
        .function_call_output_text(call_id)
        .unwrap_or_else(|| panic!("exec_command {call_id} should return model-visible text"));
    assert!(
        output.contains("Process exited with code 0"),
        "exec_command {call_id} should exit successfully: {output}"
    );
}

async fn wait_for_exec_start(
    test: &TestCodex,
    turn_id: &str,
    marker: &str,
    turn_completed: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let event = wait_for_event(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event) => event.call_id == "call-2" && event.turn_id == turn_id,
        EventMsg::TurnComplete(event) => event.turn_id == turn_id,
        _ => false,
    })
    .await;
    match event {
        EventMsg::ExecCommandBegin(event) => {
            anyhow::ensure!(
                event.command.iter().any(|part| part.contains(marker)),
                "exec_command begin did not contain marker {marker:?}"
            );
        }
        EventMsg::TurnComplete(_) => {
            turn_completed.store(true, Ordering::Release);
            anyhow::bail!("turn completed before exec_command began");
        }
        _ => unreachable!("wait predicate only accepts exec begin or turn complete"),
    }
    Ok(())
}

async fn submit_turn(test: &TestCodex, prompt: &str) -> anyhow::Result<String> {
    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());

    let submission = test
        .codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    let turn_id = match submission {
        TurnInputSubmission::Started { turn_id } => turn_id,
        TurnInputSubmission::Steered { turn_id } => {
            anyhow::bail!("expected a new turn, but input steered active turn {turn_id}")
        }
        TurnInputSubmission::NotSubmitted { reason } => {
            anyhow::bail!("expected a new turn, but input was not submitted: {reason:?}")
        }
    };

    Ok(turn_id)
}

pub(super) async fn run_turn(test: &TestCodex, prompt: &str) -> anyhow::Result<String> {
    let turn_id = submit_turn(test, prompt).await?;
    wait_for_event(&test.codex, |event| match event {
        EventMsg::TurnComplete(event) => event.turn_id == turn_id,
        _ => false,
    })
    .await;
    Ok(turn_id)
}

pub(super) async fn build_parallel_fixture(
    server: &wiremock::MockServer,
    model: &str,
) -> anyhow::Result<ParallelFixture> {
    let (extensions, barrier) = parallel_start_extension_builder();
    let mut builder = test_codex()
        .with_model(model)
        .with_extensions(Arc::new(extensions.build()));
    let test = builder.build_with_auto_env(server).await?;
    Ok(ParallelFixture { test, barrier })
}

pub(super) async fn build_mixed_parallel_fixture(
    server: &wiremock::MockServer,
) -> anyhow::Result<MixedParallelFixture> {
    let (mut extensions, barrier) = parallel_start_extension_builder();
    let (custom_started_tx, custom_started) = oneshot::channel();
    let (release_custom, custom_release_rx) = oneshot::channel();
    let tool = Arc::new(ParallelReleaseTool {
        started: Mutex::new(Some(custom_started_tx)),
        release: Mutex::new(Some(custom_release_rx)),
    });
    extensions.tool_contributor(Arc::new(ParallelReleaseToolContributor { tool }));
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_extensions(Arc::new(extensions.build()));
    let test = builder.build_with_auto_env(server).await?;
    Ok(MixedParallelFixture {
        parallel: ParallelFixture { test, barrier },
        custom_started: Some(custom_started),
        release_custom: Some(release_custom),
        exec_marker: format!("parallel-mixed-exec-{}", Uuid::new_v4()),
    })
}
