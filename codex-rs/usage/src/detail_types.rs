use crate::AccountProfileRef;
use crate::RepositoryId;
use crate::ThreadId;
use crate::UsagePageRequest;
use crate::UtcTimeRange;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageDetailKind {
    /// `process_instances` without the prohibited `os_pid` column.
    Processes,
    /// `threads`.
    Threads,
    /// `turns`, with account references resolved by the caller.
    Turns,
    /// `agents`.
    Agents,
    /// `operations`, terminal `operation_events`, `model_requests`, and `tool_invocations`.
    Operations,
    /// `token_observations`.
    Tokens,
    /// `tool_approval_events`.
    Approvals,
    /// `repository_attributions`.
    RepositoryAttributions,
    /// Append-only `classification_events`, including effective/superseded state.
    Classifications,
    /// `coverage_events`.
    Coverage,
    /// `activity_spans`, including terminal time and heartbeat count.
    ActivitySpans,
    /// Process/thread/turn/agent/activity-span lifecycle event tables.
    LifecycleEvents,
    /// Raw privacy-preserving `repositories` entities, including merged sources.
    RepositoryIdentities,
    /// Repository seen, alias, and merge event tables.
    RepositoryEvents,
    /// `taxonomy_versions`.
    Taxonomies,
}

impl UsageDetailKind {
    pub const ALL: [Self; 15] = [
        Self::Processes,
        Self::Threads,
        Self::Turns,
        Self::Agents,
        Self::Operations,
        Self::Tokens,
        Self::Approvals,
        Self::RepositoryAttributions,
        Self::Classifications,
        Self::Coverage,
        Self::ActivitySpans,
        Self::LifecycleEvents,
        Self::RepositoryIdentities,
        Self::RepositoryEvents,
        Self::Taxonomies,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Processes => "processes",
            Self::Threads => "threads",
            Self::Turns => "turns",
            Self::Agents => "agents",
            Self::Operations => "operations",
            Self::Tokens => "tokens",
            Self::Approvals => "approvals",
            Self::RepositoryAttributions => "repository_attributions",
            Self::Classifications => "classifications",
            Self::Coverage => "coverage",
            Self::ActivitySpans => "activity_spans",
            Self::LifecycleEvents => "lifecycle_events",
            Self::RepositoryIdentities => "repository_identities",
            Self::RepositoryEvents => "repository_events",
            Self::Taxonomies => "taxonomies",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "processes" => Some(Self::Processes),
            "threads" => Some(Self::Threads),
            "turns" => Some(Self::Turns),
            "agents" => Some(Self::Agents),
            "operations" => Some(Self::Operations),
            "tokens" => Some(Self::Tokens),
            "approvals" => Some(Self::Approvals),
            "repository_attributions" => Some(Self::RepositoryAttributions),
            "classifications" => Some(Self::Classifications),
            "coverage" => Some(Self::Coverage),
            "activity_spans" => Some(Self::ActivitySpans),
            "lifecycle_events" => Some(Self::LifecycleEvents),
            "repository_identities" => Some(Self::RepositoryIdentities),
            "repository_events" => Some(Self::RepositoryEvents),
            "taxonomies" => Some(Self::Taxonomies),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageDetailListQuery {
    pub page: UsagePageRequest,
    pub time_range: Option<UtcTimeRange>,
    pub thread_id: Option<ThreadId>,
    pub repository_id: Option<RepositoryId>,
    pub account_profile_ref: Option<AccountProfileRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum UsageDetailRecord {
    Process {
        id: String,
        started_at_ms: i64,
    },
    Thread {
        id: String,
        parent_thread_id: Option<String>,
        source_kind: String,
        created_at_ms: i64,
    },
    Turn {
        id: String,
        thread_id: String,
        account: Option<String>,
        account_auth_mode: Option<String>,
        created_at_ms: i64,
    },
    Agent {
        id: String,
        thread_id: String,
        parent_agent_id: Option<String>,
        role_kind: String,
        created_at_ms: i64,
    },
    Operation(Box<UsageOperationDetail>),
    Token {
        id: String,
        model_request_id: Option<String>,
        tool_invocation_id: Option<String>,
        source_event_id: String,
        category: String,
        count: Option<i64>,
        unit: String,
        measurement_provenance: String,
        coverage: String,
        repository_bucket: String,
        observed_at_ms: i64,
    },
    Approval {
        event_id: String,
        tool_invocation_id: String,
        outcome: String,
        provenance: String,
        occurred_at_ms: i64,
    },
    RepositoryAttribution {
        event_id: String,
        operation_id: String,
        repository_id: Option<String>,
        attribution_kind: String,
        provenance: String,
        occurred_at_ms: i64,
    },
    Classification {
        event_id: String,
        operation_id: String,
        taxonomy_version: i64,
        phase: String,
        activity: String,
        activity_state: String,
        provenance: String,
        supersedes_event_id: Option<String>,
        occurred_at_ms: i64,
        effective: bool,
    },
    Coverage {
        event_id: String,
        operation_id: Option<String>,
        scope_kind: String,
        coverage: String,
        reason_code: Option<String>,
        occurred_at_ms: i64,
    },
    ActivitySpan {
        id: String,
        operation_id: String,
        activity_state: String,
        started_at_ms: i64,
        ended_at_ms: Option<i64>,
        heartbeat_count: u64,
    },
    LifecycleEvent {
        event_id: String,
        owner_type: String,
        owner_id: String,
        thread_id: Option<String>,
        event: String,
        occurred_at_ms: i64,
    },
    RepositoryIdentity {
        id: String,
        identity_source: String,
        safe_display_label: String,
        created_at_ms: i64,
    },
    RepositoryEvent {
        event_id: String,
        event: String,
        repository_id: String,
        target_repository_id: Option<String>,
        safe_alias: Option<String>,
        occurred_at_ms: i64,
    },
    Taxonomy {
        version: i64,
        schema_migration: i64,
        mapping_key: String,
        supersedes_version: Option<i64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageOperationDetail {
    pub id: String,
    pub process_id: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_operation_id: Option<String>,
    pub retry_of_operation_id: Option<String>,
    pub rework_of_operation_id: Option<String>,
    pub operation_kind: String,
    pub started_at_ms: i64,
    pub taxonomy_version: i64,
    pub phase: String,
    pub activity: String,
    pub activity_state: String,
    pub attribution_provenance: String,
    pub account: Option<String>,
    pub account_auth_mode: Option<String>,
    pub terminal_event_id: Option<String>,
    pub terminal_status: Option<String>,
    pub completed_at_ms: Option<i64>,
    pub duration_ns: Option<u64>,
    pub error_category: Option<String>,
    pub model_request: Option<UsageModelRequestDetail>,
    pub tool: Option<UsageToolDetail>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelRequestDetail {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub transport: String,
    pub attempt_number: u32,
    pub account: Option<String>,
    pub account_auth_mode: Option<String>,
    pub client_origin: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageToolDetail {
    pub id: String,
    pub tool_kind: String,
    pub safe_tool_name: String,
    pub operation_family: String,
    pub observation_timing: String,
    pub covering_model_request_id: Option<String>,
}
