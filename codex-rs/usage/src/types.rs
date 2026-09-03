use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

pub const TAXONOMY_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("usage identifier is not a bounded non-content identifier")]
pub struct UsageIdentifierError;

fn valid_non_content_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !matches!(value, "." | "..")
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

macro_rules! redacted_identifier {
    ($name:ident) => {
        #[derive(Clone, Eq, Hash, PartialEq)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, UsageIdentifierError> {
                let value = value.into();
                valid_non_content_identifier(&value)
                    .then_some(Self(value))
                    .ok_or(UsageIdentifierError)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "([redacted])"))
            }
        }
    };
}

redacted_identifier!(ThreadId);
redacted_identifier!(TurnId);
redacted_identifier!(AgentId);
redacted_identifier!(ProviderKind);
redacted_identifier!(ModelName);
redacted_identifier!(TransportKind);
redacted_identifier!(ToolKind);
redacted_identifier!(ToolName);
redacted_identifier!(OperationFamily);
redacted_identifier!(ObservationTiming);
redacted_identifier!(CoverageScopeKind);
redacted_identifier!(CoverageReasonCode);
redacted_identifier!(ThreadSourceKind);
redacted_identifier!(AgentRoleKind);
redacted_identifier!(AccountProfileRef);
redacted_identifier!(ClientOrigin);

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

string_enum!(Phase {
    Planning => "planning",
    Implementation => "implementation",
    Testing => "testing",
    Deployment => "deployment",
    Reporting => "reporting",
    Unattributed => "unattributed",
});

string_enum!(Activity {
    Requirements => "requirements",
    Specification => "specification",
    RepositoryAnalysis => "repository_analysis",
    Research => "research",
    Diagnosis => "diagnosis",
    ArchitectureDesign => "architecture_design",
    WorkPlanning => "work_planning",
    Coding => "coding",
    Configuration => "configuration",
    Refactoring => "refactoring",
    DependencyOrBuildChange => "dependency_or_build_change",
    TestAuthoring => "test_authoring",
    DocumentationAuthoring => "documentation_authoring",
    DataOrSchemaChange => "data_or_schema_change",
    BuildValidation => "build_validation",
    UnitTesting => "unit_testing",
    IntegrationTesting => "integration_testing",
    BrowserQa => "browser_qa",
    CompatibilityTesting => "compatibility_testing",
    MigrationRehearsal => "migration_rehearsal",
    VerificationReview => "verification_review",
    Packaging => "packaging",
    Deployment => "deployment",
    Rollback => "rollback",
    RuntimeOperations => "runtime_operations",
    Monitoring => "monitoring",
    UserElaboration => "user_elaboration",
    StatusUpdate => "status_update",
    CompletionHandoff => "completion_handoff",
    ReviewFeedback => "review_feedback",
    Coordination => "coordination",
    AccountingOverhead => "accounting_overhead",
    Mixed => "mixed",
    Unknown => "unknown",
});

string_enum!(ActivityState {
    ModelActive => "model_active",
    ToolActive => "tool_active",
    ExternalWait => "external_wait",
    UserWait => "user_wait",
    BlockedWait => "blocked_wait",
});

string_enum!(AttributionProvenance {
    AgentDeclared => "agent_declared",
    DeterministicClassification => "deterministic_classification",
    InferredClassification => "inferred_classification",
    UserCorrected => "user_corrected",
    Unknown => "unknown",
});

string_enum!(MeasurementProvenance {
    ProviderReported => "provider_reported",
    RuntimeObserved => "runtime_observed",
    Imported => "imported",
    Unknown => "unknown",
});

string_enum!(TokenUnit {
    Tokens => "tokens",
});

string_enum!(OperationKind {
    ModelRequest => "model_request",
    LocalTool => "local_tool",
    HostedTool => "hosted_tool",
    ActivityControl => "activity_control",
});

string_enum!(ToolExecutionRole {
    Standalone => "standalone",
    Wrapper => "wrapper",
    Nested => "nested",
});

string_enum!(TerminalStatus {
    Completed => "completed",
    Incomplete => "incomplete",
    Failed => "failed",
    Denied => "denied",
    TimedOut => "timed_out",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
});

string_enum!(ApprovalOutcome {
    NotRequired => "not_required",
    Approved => "approved",
    Denied => "denied",
    TimedOut => "timed_out",
    Cancelled => "cancelled",
});

string_enum!(ApprovalProvenance {
    Policy => "policy",
    Cache => "cache",
    PermissionHook => "permission_hook",
    Guardian => "guardian",
    User => "user",
    Unknown => "unknown",
});

string_enum!(ErrorCategory {
    Authentication => "authentication",
    RateLimit => "rate_limit",
    Timeout => "timeout",
    Cancelled => "cancelled",
    Transport => "transport",
    Provider => "provider",
    Tool => "tool",
    Database => "database",
    Unavailable => "unavailable",
    Unknown => "unknown",
});

// Content-free authentication mode captured at an operation boundary.
string_enum!(AccountAuthMode {
    ApiKey => "api_key",
    Chatgpt => "chatgpt",
    ChatgptAuthTokens => "chatgpt_auth_tokens",
    Headers => "headers",
    AgentIdentity => "agent_identity",
    PersonalAccessToken => "personal_access_token",
    BedrockApiKey => "bedrock_api_key",
    BedrockAccessKeys => "bedrock_access_keys",
});

/// Immutable account attribution captured for a turn or provider request.
#[derive(Clone, Eq, PartialEq)]
pub struct AccountAttributionSnapshot {
    profile_ref: Option<AccountProfileRef>,
    auth_mode: Option<AccountAuthMode>,
}

impl AccountAttributionSnapshot {
    pub const fn unknown() -> Self {
        Self {
            profile_ref: None,
            auth_mode: None,
        }
    }

    pub const fn new(
        profile_ref: Option<AccountProfileRef>,
        auth_mode: Option<AccountAuthMode>,
    ) -> Self {
        Self {
            profile_ref,
            auth_mode,
        }
    }

    pub fn profile_ref(&self) -> Option<&AccountProfileRef> {
        self.profile_ref.as_ref()
    }

    pub const fn auth_mode(&self) -> Option<AccountAuthMode> {
        self.auth_mode
    }
}

impl Default for AccountAttributionSnapshot {
    fn default() -> Self {
        Self::unknown()
    }
}

impl fmt::Debug for AccountAttributionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountAttributionSnapshot")
            .field(
                "profile_ref",
                &self.profile_ref.as_ref().map(|_| "[redacted]"),
            )
            .field("auth_mode", &self.auth_mode)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct OperationId(Uuid);

impl OperationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_string(value: &str) -> Option<Self> {
        Uuid::parse_str(value).ok().map(Self)
    }

    pub fn from_stable_key(value: &[u8]) -> Self {
        Self(Uuid::new_v5(&Uuid::NAMESPACE_OID, value))
    }

    pub fn as_string(self) -> String {
        self.0.to_string()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ActivitySpanId(Uuid);

impl ActivitySpanId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn as_string(self) -> String {
        self.0.to_string()
    }
}

impl Default for ActivitySpanId {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ToolExecutionGroupId(Uuid);

impl ToolExecutionGroupId {
    pub fn from_stable_key(value: &[u8]) -> Self {
        Self(Uuid::new_v5(&Uuid::NAMESPACE_OID, value))
    }

    pub fn from_string(value: &str) -> Option<Self> {
        Uuid::parse_str(value).ok().map(Self)
    }

    pub fn as_string(self) -> String {
        self.0.to_string()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ProcessId(Uuid);

impl ProcessId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn as_string(self) -> String {
        self.0.to_string()
    }
}

impl Default for ProcessId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewOperation {
    pub id: OperationId,
    pub process_id: ProcessId,
    pub thread_id: Option<ThreadId>,
    pub turn_id: Option<TurnId>,
    pub agent_id: Option<AgentId>,
    pub parent_operation_id: Option<OperationId>,
    pub retry_of_operation_id: Option<OperationId>,
    pub rework_of_operation_id: Option<OperationId>,
    pub kind: OperationKind,
    pub started_at_ms: i64,
    pub phase: Phase,
    pub activity: Activity,
    pub activity_state: ActivityState,
    pub attribution_provenance: AttributionProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationLinks {
    pub retry_of_operation_id: Option<OperationId>,
    pub rework_of_operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalOperation {
    pub operation_id: OperationId,
    pub status: TerminalStatus,
    pub occurred_at_ms: i64,
    pub duration_ns: u64,
    pub error_category: Option<ErrorCategory>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub integrity: String,
    pub migration_count: u64,
    pub incomplete_operations: u64,
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
