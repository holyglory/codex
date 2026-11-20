//! Content-free approval observations at the existing reviewer/cache boundary.
use super::CaptureContext;
use super::TurnCapture;
use super::bounded_label;
use crate::session::turn_context::TurnContext;
use crate::tools::ApprovalResolutionSource;
use codex_protocol::protocol::NetworkPolicyRuleAction;
use codex_protocol::protocol::ReviewDecision;
use codex_usage::UsageApprovalDecision;
use codex_usage::UsageApprovalSource;
use codex_usage::UsageOutcome;
use codex_usage::UsageRecord;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy)]
enum ResolutionStage {
    Final,
    Preapproved,
    Policy,
    Interrupted,
}

pub(crate) struct ApprovalObservation {
    context: Arc<CaptureContext>,
    record: UsageRecord,
    started: Instant,
    finished: bool,
}

impl ApprovalObservation {
    pub(crate) fn start(turn: &TurnContext, call_id: &str, tool: &str) -> Option<Self> {
        let capture = turn.extension_data.get::<TurnCapture>()?;
        let context = Arc::clone(&capture.context);
        let id = uuid::Uuid::new_v4();
        let mut record = context.fact(format!("{id}:requested"), "tool_approval_requested");
        record.operation_id = context
            .tool_operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned();
        record.tool = Some(bounded_label(tool));
        record.started_at = Some(record.observed_at);
        record.started_at_ms = record.observed_at_ms;
        if record.operation_id.is_none() {
            record.coverage = Some("unlinked_operation".to_string());
        }
        context.sink.submit(record.clone());
        Some(Self {
            context,
            record,
            started: Instant::now(),
            finished: false,
        })
    }

    pub(crate) fn resolve(
        &mut self,
        decision: &ReviewDecision,
        source: Option<ApprovalResolutionSource>,
    ) {
        let decision = match decision {
            ReviewDecision::Approved => UsageApprovalDecision::Approved,
            ReviewDecision::ApprovedForSession => UsageApprovalDecision::ApprovedForSession,
            ReviewDecision::ApprovedExecpolicyAmendment { .. } => {
                UsageApprovalDecision::ApprovedExecpolicyAmendment
            }
            ReviewDecision::ApprovedMcpPolicyAmendment => {
                UsageApprovalDecision::ApprovedMcpPolicyAmendment
            }
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } => match network_policy_amendment.action {
                NetworkPolicyRuleAction::Allow => UsageApprovalDecision::NetworkAllowed,
                NetworkPolicyRuleAction::Deny => UsageApprovalDecision::NetworkDenied,
            },
            ReviewDecision::Denied { .. } => UsageApprovalDecision::Denied,
            ReviewDecision::TimedOut => UsageApprovalDecision::TimedOut,
            ReviewDecision::Abort => UsageApprovalDecision::Cancelled,
        };
        let (source, stage) = match source {
            Some(ApprovalResolutionSource::Hook) => {
                (Some(UsageApprovalSource::Config), ResolutionStage::Final)
            }
            Some(ApprovalResolutionSource::Guardian) => (
                Some(UsageApprovalSource::AutomatedReviewer),
                ResolutionStage::Final,
            ),
            Some(ApprovalResolutionSource::User) => {
                (Some(UsageApprovalSource::User), ResolutionStage::Final)
            }
            Some(ApprovalResolutionSource::SessionCache) => (
                Some(UsageApprovalSource::SessionCache),
                ResolutionStage::Final,
            ),
            Some(ApprovalResolutionSource::Preapproved) => (None, ResolutionStage::Preapproved),
            Some(ApprovalResolutionSource::Policy) => (None, ResolutionStage::Policy),
            None => (None, ResolutionStage::Final),
        };
        self.finish(Some(decision), source, stage);
    }

    fn finish(
        &mut self,
        decision: Option<UsageApprovalDecision>,
        source: Option<UsageApprovalSource>,
        stage: ResolutionStage,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut record = self.record.clone();
        record.id = record.id.replace(":requested", ":resolved");
        let now = super::now_ms();
        record.observed_at = now.div_euclid(1_000);
        record.observed_at_ms = Some(now);
        record.ended_at = Some(record.observed_at);
        record.ended_at_ms = Some(now);
        record.duration_ms =
            Some(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX));
        record.approval_decision = decision;
        record.approval_source = source;
        let event = match stage {
            ResolutionStage::Final if source == Some(UsageApprovalSource::SessionCache) => {
                "tool_approval_bypassed"
            }
            ResolutionStage::Final | ResolutionStage::Policy => "tool_approval_resolved",
            ResolutionStage::Preapproved => "tool_approval_bypassed",
            ResolutionStage::Interrupted => "tool_approval_interrupted",
        };
        record.event = Some(event.to_string());
        match stage {
            ResolutionStage::Preapproved => {
                record.coverage = Some("permissions_preapproved".to_string())
            }
            ResolutionStage::Policy => record.coverage = Some("policy_decision".to_string()),
            ResolutionStage::Interrupted => {
                record.coverage = Some("decision_unobserved".to_string())
            }
            ResolutionStage::Final => {}
        }
        // Dropping an unfinished observation proves no reviewer verdict.
        record.outcome = decision.map(|decision| match decision {
            UsageApprovalDecision::Approved
            | UsageApprovalDecision::ApprovedForSession
            | UsageApprovalDecision::ApprovedExecpolicyAmendment
            | UsageApprovalDecision::ApprovedMcpPolicyAmendment
            | UsageApprovalDecision::NetworkAllowed => UsageOutcome::Completed,
            UsageApprovalDecision::NetworkDenied | UsageApprovalDecision::Denied => {
                UsageOutcome::Blocked
            }
            UsageApprovalDecision::TimedOut => UsageOutcome::Incomplete,
            UsageApprovalDecision::Cancelled => UsageOutcome::Cancelled,
        });
        self.context.sink.submit(record);
    }
}

impl Drop for ApprovalObservation {
    fn drop(&mut self) {
        self.finish(
            /*decision*/ None,
            /*source*/ None,
            ResolutionStage::Interrupted,
        );
    }
}
