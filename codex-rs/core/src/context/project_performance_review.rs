use codex_event_subscriptions::ProjectAutomation;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use super::ContextualUserFragment;

const MAX_FRAGMENT_BYTES: usize = 2048;
const OPEN_TAG: &str = "<project_performance_review>";
const CLOSE_TAG: &str = "</project_performance_review>";
const INSTRUCTIONS: &str = "Automated project performance review. Do not read the parent's conversation, spawn agents, or create another scheduler. Use these bounded references, repository instructions and Coordinator decisions/outcomes. Read project_automation status; stop for a stale or paused job. Fetch usage_stats performance_review and Coordinator review prepare for exactly this UTC window. Resolve Coordinator repositoryId from cwd, not the native project hash. Identify avoidable delay, compare alternatives and apply already-authorized workflow improvements inside this repository. Writes are restricted to this repository and never exceed the parent's permissions. Specification-only work does not authorize implementing the product. Prefer speed to the next useful result; include change and verification costs. Preserve controls, acceptance criteria and active frozen runs; isolate changes when required. No unapproved installation, publication or cross-repository changes. Link outcome/experiment refs once with project_automation link_work. Obtain before/after evidence and record retained, reverted, inconclusive or justified unchanged. Never invent improvement. Complete only with project_automation complete_review, this job ID, verified RECORD@REV decision_ref and fresh expected_revision. Totals or a final message alone are not completion. Keep raw logs out of context.";

#[derive(Clone, Serialize)]
struct ReviewReferences {
    project_id: String,
    job_id: Uuid,
    signal_ref: Option<String>,
    owner_thread_id: ThreadId,
    window_start_ms: i64,
    window_end_ms: i64,
    usage_stats: UsageReference,
    coordinator: &'static str,
}

#[derive(Clone, Serialize)]
struct UsageReference {
    action: &'static str,
    repository: &'static str,
    from_at_ms: i64,
    to_at_ms: i64,
}

#[derive(Deserialize)]
struct DeliveredSignal {
    job_id: Uuid,
    signal_ref: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ProjectPerformanceReview {
    references: ReviewReferences,
    body: String,
}

impl ProjectPerformanceReview {
    pub(crate) fn new(project: &ProjectAutomation) -> Result<Self, String> {
        let job = project
            .review
            .as_ref()
            .ok_or("project review is no longer pending")?;
        let references = ReviewReferences {
            project_id: project.project_id.clone(),
            job_id: job.id,
            signal_ref: job.decision_ref.clone(),
            owner_thread_id: project.owner_thread_id,
            window_start_ms: project.review_window_start_ms,
            window_end_ms: job.due_at_ms,
            usage_stats: UsageReference {
                action: "performance_review",
                repository: "current",
                from_at_ms: project.review_window_start_ms,
                to_at_ms: job.due_at_ms,
            },
            coordinator: "review prepare",
        };
        let encoded = serde_json::to_string(&references).map_err(|error| error.to_string())?;
        let body = format!("\n{INSTRUCTIONS}\n{encoded}\n");
        if OPEN_TAG.len() + body.len() + CLOSE_TAG.len() > MAX_FRAGMENT_BYTES {
            return Err("project review references exceed the 2 KiB context bound".into());
        }
        Ok(Self { references, body })
    }

    pub(crate) fn matches_signal(&self, item: &ResponseItem) -> bool {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };
        content.iter().any(|item| {
            let ContentItem::InputText { text } = item else {
                return false;
            };
            if text.len() > MAX_FRAGMENT_BYTES {
                return false;
            }
            let Some(body) = text
                .strip_prefix(OPEN_TAG)
                .and_then(|body| body.strip_suffix(CLOSE_TAG))
            else {
                return false;
            };
            let Some((_, encoded)) = body.trim().rsplit_once('\n') else {
                return false;
            };
            serde_json::from_str::<DeliveredSignal>(encoded).is_ok_and(|previous| {
                previous.job_id == self.references.job_id
                    && previous.signal_ref == self.references.signal_ref
            })
        })
    }
}

impl ContextualUserFragment for ProjectPerformanceReview {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("project.performance_review".into())
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (OPEN_TAG, CLOSE_TAG)
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}
