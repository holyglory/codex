use crate::AccountProfileRef;
use crate::Activity;
use crate::ActivityState;
use crate::AgentId;
use crate::AttributionProvenance;
use crate::CoverageState;
use crate::FactEventId;
use crate::OperationFamily;
use crate::OperationId;
use crate::Phase;
use crate::RepositoryId;
use crate::TerminalStatus;
use crate::ThreadId;
use crate::ToolInvocationId;
use crate::ToolName;
use crate::UtcTimeRange;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsagePageCursor {
    occurred_at_ms: i64,
    id: String,
}

impl UsagePageCursor {
    pub fn new(occurred_at_ms: i64, id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        (!id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
            .then_some(Self { occurred_at_ms, id })
    }

    pub fn occurred_at_ms(&self) -> i64 {
        self.occurred_at_ms
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsagePageRequest {
    pub cursor: Option<UsagePageCursor>,
    pub limit: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsagePage<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<UsagePageCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageRepositoryRecord {
    pub id: RepositoryId,
    pub label: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageThreadRecord {
    pub id: ThreadId,
    pub repository_ids: Vec<RepositoryId>,
    pub account_profile_ref: Option<AccountProfileRef>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageToolListQuery {
    pub page: UsagePageRequest,
    pub time_range: Option<UtcTimeRange>,
    pub thread_id: Option<ThreadId>,
    pub repository_id: Option<RepositoryId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageToolRecord {
    pub id: ToolInvocationId,
    pub thread_id: ThreadId,
    pub repository_id: Option<RepositoryId>,
    pub tool_name: ToolName,
    pub operation_family: OperationFamily,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub status: Option<TerminalStatus>,
    pub provenance: AttributionProvenance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageActivityListQuery {
    pub page: UsagePageRequest,
    pub time_range: Option<UtcTimeRange>,
    pub thread_id: Option<ThreadId>,
    pub agent_id: Option<AgentId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageActivityRecord {
    pub id: OperationId,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
    pub phase: Phase,
    pub activity: Activity,
    pub state: ActivityState,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub provenance: AttributionProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageEventKind {
    ModelRequestStarted,
    ModelRequestCompleted,
    ToolStarted,
    ToolCompleted,
    ActivityChanged,
    ClassificationCorrected,
    CoverageGap,
}

impl UsageEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelRequestStarted => "model_request_started",
            Self::ModelRequestCompleted => "model_request_completed",
            Self::ToolStarted => "tool_started",
            Self::ToolCompleted => "tool_completed",
            Self::ActivityChanged => "activity_changed",
            Self::ClassificationCorrected => "classification_corrected",
            Self::CoverageGap => "coverage_gap",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "model_request_started" => Some(Self::ModelRequestStarted),
            "model_request_completed" => Some(Self::ModelRequestCompleted),
            "tool_started" => Some(Self::ToolStarted),
            "tool_completed" => Some(Self::ToolCompleted),
            "activity_changed" => Some(Self::ActivityChanged),
            "classification_corrected" => Some(Self::ClassificationCorrected),
            "coverage_gap" => Some(Self::CoverageGap),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageEventProvenance {
    ProviderReported,
    RuntimeObserved,
    AgentDeclared,
    DeterministicClassification,
    InferredClassification,
    UserCorrected,
    Imported,
    Unknown,
}

impl UsageEventProvenance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider_reported",
            Self::RuntimeObserved => "runtime_observed",
            Self::AgentDeclared => "agent_declared",
            Self::DeterministicClassification => "deterministic_classification",
            Self::InferredClassification => "inferred_classification",
            Self::UserCorrected => "user_corrected",
            Self::Imported => "imported",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "provider_reported" => Some(Self::ProviderReported),
            "runtime_observed" => Some(Self::RuntimeObserved),
            "agent_declared" => Some(Self::AgentDeclared),
            "deterministic_classification" => Some(Self::DeterministicClassification),
            "inferred_classification" => Some(Self::InferredClassification),
            "user_corrected" => Some(Self::UserCorrected),
            "imported" => Some(Self::Imported),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageEventListQuery {
    pub page: UsagePageRequest,
    pub time_range: Option<UtcTimeRange>,
    pub thread_id: Option<ThreadId>,
    pub repository_id: Option<RepositoryId>,
    pub kind: Option<UsageEventKind>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageEventRecord {
    pub id: FactEventId,
    pub thread_id: Option<ThreadId>,
    pub repository_id: Option<RepositoryId>,
    pub occurred_at_ms: i64,
    pub kind: UsageEventKind,
    pub provenance: UsageEventProvenance,
    pub coverage: CoverageState,
}
