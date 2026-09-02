use crate::repository::RepositoryId;
use crate::store::UsageStore;
use crate::store::UsageStoreError;
use crate::types::AccountProfileRef;
use crate::types::Activity;
use crate::types::ActivityState;
use crate::types::ApprovalOutcome;
use crate::types::ApprovalProvenance;
use crate::types::AttributionProvenance;
use crate::types::ClientOrigin;
use crate::types::CoverageReasonCode;
use crate::types::CoverageScopeKind;
use crate::types::MeasurementProvenance;
use crate::types::ModelName;
use crate::types::ObservationTiming;
use crate::types::OperationFamily;
use crate::types::OperationId;
use crate::types::OperationKind;
use crate::types::Phase;
use crate::types::ProviderKind;
use crate::types::TAXONOMY_VERSION;
use crate::types::TokenUnit;
use crate::types::ToolExecutionGroupId;
use crate::types::ToolExecutionRole;
use crate::types::ToolKind;
use crate::types::ToolName;
use crate::types::TransportKind;
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn from_string(value: &str) -> Option<Self> {
                Uuid::parse_str(value).ok().map(Self)
            }

            pub fn as_string(self) -> String {
                self.0.to_string()
            }

            pub fn from_stable_key(value: &[u8]) -> Self {
                Self(Uuid::new_v5(&Uuid::NAMESPACE_OID, value))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

uuid_id!(FactEventId);
uuid_id!(ModelRequestId);
uuid_id!(ToolInvocationId);

pub const MODEL_REQUEST_CONTEXT_ESTIMATOR: &str = "approx_model_visible_v1";

impl FactEventId {
    pub fn from_provider_source_key(key: &[u8; 32]) -> Self {
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&key[..16]);
        Self(Uuid::from_bytes(bytes))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FactValidationError {
    #[error("token category path is invalid")]
    InvalidTokenCategory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenCategoryPath(String);

impl TokenCategoryPath {
    pub fn new(value: impl Into<String>) -> Result<Self, FactValidationError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 192
            && value.split('.').all(|segment| {
                !segment.is_empty()
                    && segment.len() <= 64
                    && segment.bytes().enumerate().all(|(index, byte)| match byte {
                        b'a'..=b'z' => true,
                        b'0'..=b'9' | b'_' => index > 0,
                        _ => false,
                    })
            })
            && value
                .rsplit('.')
                .next()
                .is_some_and(|segment| segment == "tokens" || segment.ends_with("_tokens"));
        if !valid {
            return Err(FactValidationError::InvalidTokenCategory);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageState {
    CaptureStarted,
    Complete,
    Partial,
    Unknown,
    Corrupt,
    Unavailable,
    Recovery,
}

impl CoverageState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CaptureStarted => "capture_started",
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
            Self::Corrupt => "corrupt",
            Self::Unavailable => "unavailable",
            Self::Recovery => "recovery",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "capture_started" => Some(Self::CaptureStarted),
            "complete" => Some(Self::Complete),
            "partial" => Some(Self::Partial),
            "unknown" => Some(Self::Unknown),
            "corrupt" => Some(Self::Corrupt),
            "unavailable" => Some(Self::Unavailable),
            "recovery" => Some(Self::Recovery),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewModelRequest {
    pub id: ModelRequestId,
    pub operation_id: OperationId,
    pub provider_kind: ProviderKind,
    pub model: ModelName,
    pub transport_kind: TransportKind,
    pub attempt_number: u32,
    pub account: crate::types::AccountAttributionSnapshot,
    pub client_origin: ClientOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewModelRequestContext {
    pub model_request_id: ModelRequestId,
    pub policy_estimated_tokens: u64,
    pub conversation_estimated_tokens: u64,
    pub tool_output_estimated_tokens: u64,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewToolInvocation {
    pub id: ToolInvocationId,
    pub operation_id: OperationId,
    pub operation_kind: OperationKind,
    pub tool_kind: ToolKind,
    pub safe_tool_name: ToolName,
    pub operation_family: OperationFamily,
    pub observation_timing: ObservationTiming,
    pub covering_model_request_id: Option<ModelRequestId>,
    pub execution_group_id: Option<ToolExecutionGroupId>,
    pub execution_role: ToolExecutionRole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewToolApprovalEvent {
    pub event_id: FactEventId,
    pub tool_invocation_id: ToolInvocationId,
    pub outcome: ApprovalOutcome,
    pub provenance: ApprovalProvenance,
    pub occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenObservationSource {
    ModelRequest(ModelRequestId),
    ToolInvocation(ToolInvocationId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryBucket {
    Single(RepositoryId),
    MultiRepo,
    Unknown,
}

impl RepositoryBucket {
    pub(crate) fn as_string(&self) -> String {
        match self {
            Self::Single(repository_id) => repository_id.as_str().to_string(),
            Self::MultiRepo => "multi_repo".to_string(),
            Self::Unknown => "unknown".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewTokenObservation {
    pub id: FactEventId,
    pub source_event_id: FactEventId,
    pub source: TokenObservationSource,
    pub category_path: TokenCategoryPath,
    pub token_count: Option<u64>,
    pub unit: TokenUnit,
    pub measurement_provenance: MeasurementProvenance,
    pub coverage_state: CoverageState,
    pub repository_bucket: RepositoryBucket,
    pub observed_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryAttributionKind {
    Primary,
    ObservedCwd,
    FileChange,
    MultiRepo,
    Unknown,
}

impl RepositoryAttributionKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::ObservedCwd => "observed_cwd",
            Self::FileChange => "file_change",
            Self::MultiRepo => "multi_repo",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryAttributionProvenance {
    RuntimeObserved,
    Imported,
    Unknown,
}

impl RepositoryAttributionProvenance {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeObserved => "runtime_observed",
            Self::Imported => "imported",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewRepositoryAttribution {
    pub event_id: FactEventId,
    pub operation_id: OperationId,
    pub repository_id: Option<RepositoryId>,
    pub kind: RepositoryAttributionKind,
    pub provenance: RepositoryAttributionProvenance,
    pub occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewClassificationEvent {
    pub event_id: FactEventId,
    pub operation_id: OperationId,
    pub phase: Phase,
    pub activity: Activity,
    pub activity_state: ActivityState,
    pub provenance: AttributionProvenance,
    pub supersedes_event_id: Option<FactEventId>,
    pub occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewCoverageEvent {
    pub event_id: FactEventId,
    pub operation_id: Option<OperationId>,
    pub scope_kind: CoverageScopeKind,
    pub state: CoverageState,
    pub reason_code: Option<CoverageReasonCode>,
    pub occurred_at_ms: i64,
}

impl UsageStore {
    pub async fn record_model_request(
        &self,
        fact: &NewModelRequest,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO model_requests(
                id, operation_id, operation_kind, provider_kind, model,
                transport_kind, attempt_number, account_profile_ref, account_auth_mode,
                client_origin
            ) VALUES (?, ?, 'model_request', ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(fact.id.as_string())
        .bind(fact.operation_id.as_string())
        .bind(fact.provider_kind.as_str())
        .bind(fact.model.as_str())
        .bind(fact.transport_kind.as_str())
        .bind(i64::from(fact.attempt_number))
        .bind(fact.account.profile_ref().map(AccountProfileRef::as_str))
        .bind(
            fact.account
                .auth_mode()
                .map(crate::types::AccountAuthMode::as_str),
        )
        .bind(fact.client_origin.as_str())
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 && !self.model_request_matches(fact).await? {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_model_request_context(
        &self,
        fact: &NewModelRequestContext,
    ) -> Result<(), UsageStoreError> {
        let policy_estimated_tokens = i64::try_from(fact.policy_estimated_tokens)
            .map_err(|_| UsageStoreError::TokenCountOutOfRange)?;
        let conversation_estimated_tokens = i64::try_from(fact.conversation_estimated_tokens)
            .map_err(|_| UsageStoreError::TokenCountOutOfRange)?;
        let tool_output_estimated_tokens = i64::try_from(fact.tool_output_estimated_tokens)
            .map_err(|_| UsageStoreError::TokenCountOutOfRange)?;
        let model_request_id = fact.model_request_id.as_string();
        let result = sqlx::query(
            r#"
            INSERT INTO model_request_context_sources(
                model_request_id, policy_estimated_tokens,
                conversation_estimated_tokens, tool_output_estimated_tokens,
                estimator, observed_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(model_request_id) DO NOTHING
            "#,
        )
        .bind(&model_request_id)
        .bind(policy_estimated_tokens)
        .bind(conversation_estimated_tokens)
        .bind(tool_output_estimated_tokens)
        .bind(MODEL_REQUEST_CONTEXT_ESTIMATOR)
        .bind(fact.observed_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(
                    SELECT 1 FROM model_request_context_sources
                    WHERE model_request_id = ?
                      AND policy_estimated_tokens = ?
                      AND conversation_estimated_tokens = ?
                      AND tool_output_estimated_tokens = ?
                      AND estimator = ? AND observed_at_ms = ?
                )
                "#,
            )
            .bind(&model_request_id)
            .bind(policy_estimated_tokens)
            .bind(conversation_estimated_tokens)
            .bind(tool_output_estimated_tokens)
            .bind(MODEL_REQUEST_CONTEXT_ESTIMATOR)
            .bind(fact.observed_at_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_tool_invocation(
        &self,
        fact: &NewToolInvocation,
    ) -> Result<(), UsageStoreError> {
        if (fact.operation_kind == OperationKind::HostedTool)
            != fact.covering_model_request_id.is_some()
            || (fact.execution_role == ToolExecutionRole::Standalone
                && fact.execution_group_id.is_some())
        {
            return Err(UsageStoreError::InvalidFact);
        }
        let covering_model_request_id = fact
            .covering_model_request_id
            .map(ModelRequestId::as_string);
        let execution_group_id = fact.execution_group_id.map(ToolExecutionGroupId::as_string);
        let result = sqlx::query(
            r#"
            INSERT INTO tool_invocations(
                id, operation_id, operation_kind, tool_kind, safe_tool_name,
                operation_family, observation_timing, covering_model_request_id,
                execution_group_id, execution_role
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(fact.id.as_string())
        .bind(fact.operation_id.as_string())
        .bind(fact.operation_kind.as_str())
        .bind(fact.tool_kind.as_str())
        .bind(fact.safe_tool_name.as_str())
        .bind(fact.operation_family.as_str())
        .bind(fact.observation_timing.as_str())
        .bind(&covering_model_request_id)
        .bind(&execution_group_id)
        .bind(fact.execution_role.as_str())
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !self
                .tool_invocation_matches(
                    fact,
                    covering_model_request_id.as_deref(),
                    execution_group_id.as_deref(),
                )
                .await?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_tool_approval(
        &self,
        fact: &NewToolApprovalEvent,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO tool_approval_events(
                event_id, tool_invocation_id, outcome, provenance, occurred_at_ms
            ) VALUES (?, ?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING
            "#,
        )
        .bind(fact.event_id.as_string())
        .bind(fact.tool_invocation_id.as_string())
        .bind(fact.outcome.as_str())
        .bind(fact.provenance.as_str())
        .bind(fact.occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(SELECT 1 FROM tool_approval_events
                    WHERE event_id = ? AND tool_invocation_id = ?
                      AND outcome = ? AND provenance = ? AND occurred_at_ms = ?)
                "#,
            )
            .bind(fact.event_id.as_string())
            .bind(fact.tool_invocation_id.as_string())
            .bind(fact.outcome.as_str())
            .bind(fact.provenance.as_str())
            .bind(fact.occurred_at_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_token_observation(
        &self,
        fact: &NewTokenObservation,
    ) -> Result<(), UsageStoreError> {
        if (fact.token_count.is_none() && fact.coverage_state == CoverageState::Complete)
            || (fact.token_count.is_some()
                && fact.measurement_provenance == MeasurementProvenance::Unknown)
            || fact.measurement_provenance == MeasurementProvenance::RuntimeObserved
        {
            return Err(UsageStoreError::InvalidFact);
        }
        let token_count = fact
            .token_count
            .map(i64::try_from)
            .transpose()
            .map_err(|_| UsageStoreError::TokenCountOutOfRange)?;
        if let TokenObservationSource::ToolInvocation(tool_id) = &fact.source {
            let source = sqlx::query_as::<_, (String, Option<String>)>(
                r#"
                SELECT operation_kind, covering_model_request_id
                FROM tool_invocations WHERE id = ?
                "#,
            )
            .bind((*tool_id).as_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
            if !source.is_some_and(|(kind, covering)| match kind.as_str() {
                "local_tool" => covering.is_none(),
                "hosted_tool" => covering.is_some(),
                "model_request" | "activity_control" => false,
                _ => false,
            }) {
                return Err(UsageStoreError::InvalidFact);
            }
        }
        let bucket = fact.repository_bucket.as_string();
        let (model_request_id, tool_invocation_id) = match &fact.source {
            TokenObservationSource::ModelRequest(id) => (Some((*id).as_string()), None),
            TokenObservationSource::ToolInvocation(id) => (None, Some((*id).as_string())),
        };
        let result = sqlx::query(
            r#"
            INSERT INTO token_observations(
                id, model_request_id, tool_invocation_id, source_event_id,
                category_path, token_count, unit, measurement_provenance,
                coverage_state, repository_bucket, observed_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(fact.id.as_string())
        .bind(&model_request_id)
        .bind(&tool_invocation_id)
        .bind(fact.source_event_id.as_string())
        .bind(fact.category_path.as_str())
        .bind(token_count)
        .bind(fact.unit.as_str())
        .bind(fact.measurement_provenance.as_str())
        .bind(fact.coverage_state.as_str())
        .bind(&bucket)
        .bind(fact.observed_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !self
                .token_observation_matches(
                    fact,
                    token_count,
                    &bucket,
                    model_request_id.as_deref(),
                    tool_invocation_id.as_deref(),
                )
                .await?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_repository_attribution(
        &self,
        fact: &NewRepositoryAttribution,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO repository_attributions(
                event_id, operation_id, repository_id, attribution_kind,
                provenance, occurred_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING
            "#,
        )
        .bind(fact.event_id.as_string())
        .bind(fact.operation_id.as_string())
        .bind(fact.repository_id.as_ref().map(RepositoryId::as_str))
        .bind(fact.kind.as_str())
        .bind(fact.provenance.as_str())
        .bind(fact.occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 && !self.repository_attribution_matches(fact).await? {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_classification(
        &self,
        fact: &NewClassificationEvent,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO classification_events(
                event_id, operation_id, taxonomy_version, phase, activity,
                activity_state, provenance, supersedes_event_id, occurred_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING
            "#,
        )
        .bind(fact.event_id.as_string())
        .bind(fact.operation_id.as_string())
        .bind(TAXONOMY_VERSION)
        .bind(fact.phase.as_str())
        .bind(fact.activity.as_str())
        .bind(fact.activity_state.as_str())
        .bind(fact.provenance.as_str())
        .bind(fact.supersedes_event_id.map(FactEventId::as_string))
        .bind(fact.occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 && !self.classification_matches(fact).await? {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn record_coverage(&self, fact: &NewCoverageEvent) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO coverage_events(
                event_id, operation_id, scope_kind, coverage_state,
                reason_code, occurred_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING
            "#,
        )
        .bind(fact.event_id.as_string())
        .bind(fact.operation_id.map(OperationId::as_string))
        .bind(fact.scope_kind.as_str())
        .bind(fact.state.as_str())
        .bind(fact.reason_code.as_ref().map(CoverageReasonCode::as_str))
        .bind(fact.occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 && !self.coverage_matches(fact).await? {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    async fn model_request_matches(&self, fact: &NewModelRequest) -> Result<bool, UsageStoreError> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                i64,
                Option<String>,
                Option<String>,
                String,
            ),
        >(
            r#"
            SELECT operation_id, provider_kind, model, transport_kind, attempt_number,
                   account_profile_ref, account_auth_mode, client_origin
            FROM model_requests WHERE id = ?
            "#,
        )
        .bind(fact.id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(row.is_some_and(|row| {
            row == (
                fact.operation_id.as_string(),
                fact.provider_kind.as_str().to_string(),
                fact.model.as_str().to_string(),
                fact.transport_kind.as_str().to_string(),
                i64::from(fact.attempt_number),
                fact.account
                    .profile_ref()
                    .map(|profile| profile.as_str().to_string()),
                fact.account
                    .auth_mode()
                    .map(|mode| mode.as_str().to_string()),
                fact.client_origin.as_str().to_string(),
            )
        }))
    }

    async fn tool_invocation_matches(
        &self,
        fact: &NewToolInvocation,
        covering_model_request_id: Option<&str>,
        execution_group_id: Option<&str>,
    ) -> Result<bool, UsageStoreError> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                String,
            ),
        >(
            r#"
            SELECT operation_id, operation_kind, tool_kind, safe_tool_name,
                   operation_family, observation_timing, covering_model_request_id,
                   execution_group_id, execution_role
            FROM tool_invocations WHERE id = ?
            "#,
        )
        .bind(fact.id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(row.is_some_and(|row| {
            row == (
                fact.operation_id.as_string(),
                fact.operation_kind.as_str().to_string(),
                fact.tool_kind.as_str().to_string(),
                fact.safe_tool_name.as_str().to_string(),
                fact.operation_family.as_str().to_string(),
                fact.observation_timing.as_str().to_string(),
                covering_model_request_id.map(str::to_string),
                execution_group_id.map(str::to_string),
                fact.execution_role.as_str().to_string(),
            )
        }))
    }

    async fn repository_attribution_matches(
        &self,
        fact: &NewRepositoryAttribution,
    ) -> Result<bool, UsageStoreError> {
        let row = sqlx::query_as::<_, (String, Option<String>, String, String, i64)>(
            r#"
            SELECT operation_id, repository_id, attribution_kind, provenance, occurred_at_ms
            FROM repository_attributions WHERE event_id = ?
            "#,
        )
        .bind(fact.event_id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(row.is_some_and(|row| {
            row == (
                fact.operation_id.as_string(),
                fact.repository_id
                    .as_ref()
                    .map(|id| id.as_str().to_string()),
                fact.kind.as_str().to_string(),
                fact.provenance.as_str().to_string(),
                fact.occurred_at_ms,
            )
        }))
    }

    async fn classification_matches(
        &self,
        fact: &NewClassificationEvent,
    ) -> Result<bool, UsageStoreError> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                i64,
                String,
                String,
                String,
                String,
                Option<String>,
                i64,
            ),
        >(
            r#"
            SELECT operation_id, taxonomy_version, phase, activity, activity_state,
                   provenance, supersedes_event_id, occurred_at_ms
            FROM classification_events WHERE event_id = ?
            "#,
        )
        .bind(fact.event_id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(row.is_some_and(|row| {
            row == (
                fact.operation_id.as_string(),
                TAXONOMY_VERSION,
                fact.phase.as_str().to_string(),
                fact.activity.as_str().to_string(),
                fact.activity_state.as_str().to_string(),
                fact.provenance.as_str().to_string(),
                fact.supersedes_event_id.map(FactEventId::as_string),
                fact.occurred_at_ms,
            )
        }))
    }

    async fn coverage_matches(&self, fact: &NewCoverageEvent) -> Result<bool, UsageStoreError> {
        let row = sqlx::query_as::<_, (Option<String>, String, String, Option<String>, i64)>(
            r#"
            SELECT operation_id, scope_kind, coverage_state, reason_code, occurred_at_ms
            FROM coverage_events WHERE event_id = ?
            "#,
        )
        .bind(fact.event_id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(row.is_some_and(|row| {
            row == (
                fact.operation_id.map(OperationId::as_string),
                fact.scope_kind.as_str().to_string(),
                fact.state.as_str().to_string(),
                fact.reason_code
                    .as_ref()
                    .map(|reason| reason.as_str().to_string()),
                fact.occurred_at_ms,
            )
        }))
    }

    async fn token_observation_matches(
        &self,
        fact: &NewTokenObservation,
        token_count: Option<i64>,
        bucket: &str,
        model_request_id: Option<&str>,
        tool_invocation_id: Option<&str>,
    ) -> Result<bool, UsageStoreError> {
        let row = sqlx::query(
            r#"
            SELECT token_count, unit, measurement_provenance, coverage_state,
                   repository_bucket, observed_at_ms
            FROM token_observations
            WHERE model_request_id IS ? AND tool_invocation_id IS ?
              AND source_event_id = ? AND category_path = ?
            "#,
        )
        .bind(model_request_id)
        .bind(tool_invocation_id)
        .bind(fact.source_event_id.as_string())
        .bind(fact.category_path.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let Some(row) = row else {
            return Ok(false);
        };
        Ok(row.get::<Option<i64>, _>("token_count") == token_count
            && row.get::<String, _>("unit") == fact.unit.as_str()
            && row.get::<String, _>("measurement_provenance")
                == fact.measurement_provenance.as_str()
            && row.get::<String, _>("coverage_state") == fact.coverage_state.as_str()
            && row.get::<String, _>("repository_bucket") == bucket
            && row.get::<i64, _>("observed_at_ms") == fact.observed_at_ms)
    }
}

#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;
