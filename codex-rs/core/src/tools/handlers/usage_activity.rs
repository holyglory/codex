use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::usage_runtime::UsageActivityRelation;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_usage::Activity;
use codex_usage::FactEventId;
use codex_usage::Phase;
use codex_usage::RepositoryId;
use codex_usage::ThreadId;
use codex_usage::UsageStore;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub struct UsageActivityHandler;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum UsageActivityArgs {
    Set {
        phase: Phase,
        activity: Activity,
        #[serde(default)]
        relation: UsageActivityRelation,
    },
    Heartbeat,
    End,
    CorrectClassification {
        target_id: String,
        phase: Phase,
        activity: Activity,
    },
}

impl ToolExecutor<ToolInvocation> for UsageActivityHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("usage_activity")
    }

    fn spec(&self) -> ToolSpec {
        usage_activity_spec()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "usage_activity requires a function payload".to_string(),
                ));
            };
            let args: UsageActivityArgs = serde_json::from_str(arguments).map_err(|_| {
                FunctionCallError::RespondToModel(
                    "usage_activity arguments must match its enum schema".to_string(),
                )
            })?;
            let thread_id = invocation.session.thread_id().to_string();
            let response = match args {
                UsageActivityArgs::Set {
                    phase,
                    activity,
                    relation,
                } => {
                    if matches!(phase, Phase::Unattributed)
                        || matches!(
                            activity,
                            Activity::AccountingOverhead | Activity::Mixed | Activity::Unknown
                        )
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "usage_activity set requires an operational phase and activity"
                                .to_string(),
                        ));
                    }
                    invocation
                        .session
                        .services
                        .usage_runtime
                        .stage_activity(&thread_id, phase, activity, relation)
                        .await
                        .map_err(|_| {
                            FunctionCallError::RespondToModel(
                                "usage_activity rework_previous requires a prior model operation"
                                    .to_string(),
                            )
                        })?;
                    "staged".to_string()
                }
                UsageActivityArgs::Heartbeat => {
                    invocation
                        .session
                        .services
                        .usage_runtime
                        .heartbeat_activity(&thread_id)
                        .await;
                    "ok".to_string()
                }
                UsageActivityArgs::End => {
                    invocation
                        .session
                        .services
                        .usage_runtime
                        .end_activity(&thread_id)
                        .await;
                    "ended".to_string()
                }
                UsageActivityArgs::CorrectClassification {
                    target_id,
                    phase,
                    activity,
                } => {
                    if matches!(phase, Phase::Unattributed)
                        || matches!(
                            activity,
                            Activity::AccountingOverhead | Activity::Mixed | Activity::Unknown
                        )
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "usage_activity correction requires an operational phase and activity"
                                .to_string(),
                        ));
                    }
                    let target = FactEventId::from_string(&target_id).ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "usage_activity correction target_id is invalid".to_string(),
                        )
                    })?;
                    let store = UsageStore::open(&invocation.turn.config.codex_home)
                        .await
                        .map_err(|_| {
                            FunctionCallError::RespondToModel(
                                "local usage storage is unavailable".to_string(),
                            )
                        })?;
                    let event = store
                        .correct_classification(
                            target,
                            phase,
                            activity,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await
                        .map_err(|_| {
                            FunctionCallError::RespondToModel(
                                "local usage storage is unavailable".to_string(),
                            )
                        })?
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "usage classification target was not found".to_string(),
                            )
                        })?;
                    json!({
                        "schemaVersion": 1,
                        "kind": "usageClassificationCorrected",
                        "event": {
                            "id": event.id.as_string(),
                            "threadId": event.thread_id.as_ref().map(ThreadId::as_str),
                            "repositoryId": event.repository_id.as_ref().map(RepositoryId::as_str),
                            "occurredAtMs": event.occurred_at_ms,
                            "event": event.kind.as_str(),
                            "provenance": event.provenance.as_str(),
                            "coverage": event.coverage.as_str(),
                        }
                    })
                    .to_string()
                }
            };
            Ok(
                boxed_tool_output(FunctionToolOutput::from_text(response, Some(true)))
                    as Box<dyn ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for UsageActivityHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }

    fn bypasses_tool_hooks(&self) -> bool {
        true
    }
}

fn usage_activity_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("set"),
                    json!("heartbeat"),
                    json!("end"),
                    json!("correct_classification"),
                ],
                Some(
                    "Set, refresh, or end declared activity, or append a classification correction."
                        .to_string(),
                ),
            ),
        ),
        (
            "phase".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("planning"),
                    json!("implementation"),
                    json!("testing"),
                    json!("deployment"),
                    json!("reporting"),
                ],
                Some("Required for set and correct_classification.".to_string()),
            ),
        ),
        (
            "activity".to_string(),
            JsonSchema::string_enum(
                operational_activities()
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
                Some("Required for set and correct_classification.".to_string()),
            ),
        ),
        (
            "relation".to_string(),
            JsonSchema::string_enum(
                vec![json!("new_work"), json!("rework_previous")],
                Some("Optional structured relationship for set; defaults to new_work.".to_string()),
            ),
        ),
        (
            "target_id".to_string(),
            JsonSchema::string(Some(
                "Operation or related event id; required for correct_classification.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "usage_activity".to_string(),
        description: "Declare the category for upcoming work or append an enum-only correction to stored usage. Use set before the next model request, heartbeat periodically while it remains active, end when it stops, and correct_classification with target_id plus phase/activity for historical correction. Set relation=rework_previous only for an explicit redo of the prior model operation. Declarations activate only at the next model request; do not include prose or task content."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn operational_activities() -> Vec<String> {
    [
        Activity::Requirements,
        Activity::Specification,
        Activity::RepositoryAnalysis,
        Activity::Research,
        Activity::Diagnosis,
        Activity::ArchitectureDesign,
        Activity::WorkPlanning,
        Activity::Coding,
        Activity::Configuration,
        Activity::Refactoring,
        Activity::DependencyOrBuildChange,
        Activity::TestAuthoring,
        Activity::DocumentationAuthoring,
        Activity::DataOrSchemaChange,
        Activity::BuildValidation,
        Activity::UnitTesting,
        Activity::IntegrationTesting,
        Activity::BrowserQa,
        Activity::CompatibilityTesting,
        Activity::MigrationRehearsal,
        Activity::VerificationReview,
        Activity::Packaging,
        Activity::Deployment,
        Activity::Rollback,
        Activity::RuntimeOperations,
        Activity::Monitoring,
        Activity::UserElaboration,
        Activity::StatusUpdate,
        Activity::CompletionHandoff,
        Activity::ReviewFeedback,
        Activity::Coordination,
    ]
    .into_iter()
    .map(|activity| activity.as_str().to_string())
    .collect()
}
