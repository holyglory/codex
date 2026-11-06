use super::CaptureContext;
use super::TurnCapture;
use super::activity_taxonomy::Activity;
use super::activity_taxonomy::Phase;
use super::activity_taxonomy::Relation;
use super::sink::Persistence;
use crate::tools::context::ToolInvocation;
use codex_usage::UsageRecord;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Mutex;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ActivityAction {
    Set {
        phase: Phase,
        activity: Activity,
        #[serde(default)]
        relation: Relation,
    },
    Heartbeat,
    End,
    CorrectClassification {
        target_id: String,
        phase: Phase,
        activity: Activity,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityReceipt {
    schema_version: u32,
    kind: &'static str,
    status: &'static str,
    pub(crate) persistence: Persistence,
    event_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_resolution: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relation: Option<&'static str>,
}

struct Declaration {
    id: String,
    phase: Phase,
    activity: Activity,
    rework_of: Option<String>,
}

#[derive(Default)]
struct Declarations {
    staged: Option<Declaration>,
    active: Option<Declaration>,
    latest_model_operation: Option<String>,
    request_rework_of: Option<String>,
}

#[derive(Default)]
pub(super) struct ActivityState(Mutex<Declarations>);

impl Declaration {
    fn annotate(&self, record: &mut UsageRecord) {
        record.phase = Some(self.phase.as_str().to_string());
        record.activity = Some(self.activity.as_str().to_string());
        record.classification = Some("agent_declared".to_string());
    }

    fn fact(&self, context: &CaptureContext, event: &str) -> UsageRecord {
        let mut record = context.fact(uuid::Uuid::new_v4().to_string(), event);
        record.operation_id = Some(self.id.clone());
        record.rework_of_operation_id = self.rework_of.clone();
        self.annotate(&mut record);
        record
    }
}

impl ActivityState {
    pub(super) fn model_started(&self, context: &CaptureContext, record: &mut UsageRecord) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.request_rework_of = None;
        if let Some(declaration) = state.staged.take() {
            if let Some(previous) = state.active.take() {
                context
                    .sink
                    .submit(previous.fact(context, "activity_replaced"));
            }
            let mut activated = declaration.fact(context, "activity_activated");
            activated.operation_id = record.operation_id.clone();
            context.sink.submit(activated);
            state.request_rework_of = declaration.rework_of.clone();
            state.active = Some(declaration);
        }
        if let Some(active) = &state.active {
            active.annotate(record);
            record.rework_of_operation_id = state.request_rework_of.clone();
        }
        state.latest_model_operation = record.operation_id.clone();
    }

    pub(super) fn annotate(&self, record: &mut UsageRecord) {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = &state.active {
            active.annotate(record);
            record.rework_of_operation_id = state.request_rework_of.clone();
        }
    }

    pub(super) fn stop(&self, context: &CaptureContext) {
        let state = std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if let Some(active) = state.active {
            context
                .sink
                .submit(active.fact(context, "activity_stopped"));
        }
        if let Some(staged) = state.staged {
            context
                .sink
                .submit(staged.fact(context, "activity_discarded"));
        }
    }
}

pub(crate) async fn apply_activity(
    invocation: &ToolInvocation,
    action: ActivityAction,
) -> Result<ActivityReceipt, &'static str> {
    let capture = invocation
        .turn
        .extension_data
        .get::<TurnCapture>()
        .ok_or("local usage capture is unavailable")?;
    let context = &capture.context;
    let mut target_id = None;
    let mut relation = None;
    let (record, kind, status) = match action {
        ActivityAction::CorrectClassification {
            target_id: requested,
            phase,
            activity,
        } => {
            let operation = requested
                .strip_suffix(":started")
                .or_else(|| requested.strip_suffix(":finished"))
                .unwrap_or(&requested);
            let operation = uuid::Uuid::parse_str(operation)
                .map_err(|_| "usage_activity correction target_id is invalid")?
                .to_string();
            let mut record = UsageRecord::new_millis(
                uuid::Uuid::new_v4().to_string(),
                super::now_ms(),
                /*input_tokens*/ 0,
                /*output_tokens*/ 0,
            );
            record.operation_id = Some(operation.clone());
            record.event = Some("classification_correction".to_string());
            record.classification = Some("agent_corrected".to_string());
            record.coverage = Some("recorded".to_string());
            record.phase = Some(phase.as_str().to_string());
            record.activity = Some(activity.as_str().to_string());
            target_id = Some(operation);
            (record, "usageClassificationCorrection", "recorded")
        }
        action => {
            let mut state = context
                .activity
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (record, status) = match action {
                ActivityAction::Set {
                    phase,
                    activity,
                    relation: requested_relation,
                } => {
                    let rework_of = match requested_relation {
                        Relation::NewWork => None,
                        Relation::ReworkPrevious => {
                            Some(state.latest_model_operation.clone().ok_or(
                                "usage_activity rework_previous requires a prior model operation",
                            )?)
                        }
                    };
                    let declaration = Declaration {
                        id: uuid::Uuid::new_v4().to_string(),
                        phase,
                        activity,
                        rework_of,
                    };
                    let record = declaration.fact(context, "activity_staged");
                    if let Some(previous) = state.staged.replace(declaration) {
                        context
                            .sink
                            .submit(previous.fact(context, "activity_discarded"));
                    }
                    relation = Some(requested_relation.as_str());
                    (record, "staged")
                }
                ActivityAction::Heartbeat => (
                    state.active.as_ref().map_or_else(
                        || context.fact(uuid::Uuid::new_v4().to_string(), "activity_heartbeat"),
                        |active| active.fact(context, "activity_heartbeat"),
                    ),
                    if state.active.is_some() {
                        "ok"
                    } else {
                        "inactive"
                    },
                ),
                ActivityAction::End => {
                    let active = state.active.take();
                    let staged = state.staged.take();
                    if active.is_some()
                        && let Some(staged) = &staged
                    {
                        context
                            .sink
                            .submit(staged.fact(context, "activity_discarded"));
                    }
                    let declaration = active.or(staged);
                    state.request_rework_of = None;
                    (
                        declaration.map_or_else(
                            || context.fact(uuid::Uuid::new_v4().to_string(), "activity_ended"),
                            |active| active.fact(context, "activity_ended"),
                        ),
                        "ended",
                    )
                }
                ActivityAction::CorrectClassification { .. } => unreachable!(),
            };
            (record, "usageActivity", status)
        }
    };
    let event_id = record.id.clone();
    let persistence = context.sink.submit_confirmed(record).await;
    Ok(ActivityReceipt {
        schema_version: 1,
        kind,
        status: if persistence == Persistence::Recorded {
            status
        } else {
            "deferred"
        },
        persistence,
        event_id,
        target_resolution: target_id.as_ref().map(|_| "pending"),
        target_id,
        relation,
    })
}
