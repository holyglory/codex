use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageAccountingCapability {
    pub version: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsagePhase {
    Planning,
    Implementation,
    Testing,
    Deployment,
    Reporting,
    Unattributed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageActivityKind {
    Requirements,
    Specification,
    RepositoryAnalysis,
    Research,
    Diagnosis,
    ArchitectureDesign,
    WorkPlanning,
    Coding,
    Configuration,
    Refactoring,
    DependencyOrBuildChange,
    TestAuthoring,
    DocumentationAuthoring,
    DataOrSchemaChange,
    BuildValidation,
    UnitTesting,
    IntegrationTesting,
    BrowserQa,
    CompatibilityTesting,
    MigrationRehearsal,
    VerificationReview,
    Packaging,
    Deployment,
    Rollback,
    RuntimeOperations,
    Monitoring,
    UserElaboration,
    StatusUpdate,
    CompletionHandoff,
    ReviewFeedback,
    Coordination,
    AccountingOverhead,
    Mixed,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageActivityState {
    ModelActive,
    ToolActive,
    ExternalWait,
    UserWait,
    BlockedWait,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageProvenance {
    ProviderReported,
    RuntimeObserved,
    AgentDeclared,
    DeterministicClassification,
    InferredClassification,
    UserCorrected,
    Imported,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageCoverage {
    Complete,
    Partial,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageAggregate {
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub model_requests: i64,
    pub tool_calls: i64,
    pub duration_ms: i64,
    pub coverage: LocalUsageCoverage,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageTokenCategory {
    pub category_key: String,
    pub count: Option<i64>,
    pub provenance: LocalUsageProvenance,
    pub coverage: LocalUsageCoverage,
}

/// Versioned, full-fidelity local usage report.
///
/// The report preserves provider-native token categories and keeps coverage,
/// measurement, attribution, and timing facts separate so clients do not need
/// to infer unsupported totals.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReport {
    pub schema_version: u32,
    pub kind: String,
    pub database_schema_version: u64,
    pub taxonomy_version: i64,
    pub scope: LocalUsageReportScope,
    pub account: Option<String>,
    pub time_range: Option<LocalUsageReportTimeRange>,
    pub coverage: LocalUsageReportCoverage,
    pub counts: LocalUsageReportCounts,
    pub provider_tokens: Vec<LocalUsageReportTokenAggregate>,
    pub provider_tokens_by_activity: Vec<LocalUsageReportActivityTokenAggregate>,
    pub tools: LocalUsageReportToolMetrics,
    pub time: LocalUsageReportTimeMetrics,
    pub classifications: Vec<LocalUsageReportClassificationCount>,
    pub repository_participation: LocalUsageReportRepositoryParticipation,
    pub formulas: LocalUsageReportFormulas,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportScope {
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub kind: String,
    pub id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportTimeRange {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportCoverage {
    pub state: String,
    pub has_gaps: bool,
    pub events: Vec<LocalUsageReportCoverageCount>,
    pub token_observations: Vec<LocalUsageReportCoverageCount>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportCoverageCount {
    pub state: String,
    pub count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportCounts {
    pub operations: u64,
    pub model_requests: u64,
    pub tools: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportTokenAggregate {
    pub category: String,
    pub repository_bucket: String,
    /// Safe display label resolved at read time. Machine clients can continue
    /// to use `repository_bucket` as the stable privacy-preserving key.
    pub repository_label: Option<String>,
    pub measurement_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
    pub observation_count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportActivityTokenAggregate {
    pub phase: String,
    pub activity: String,
    pub attribution_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportDuration {
    pub measured_ns: u64,
    pub exact_ns: Option<u64>,
    pub unknown_intervals: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportToolMetrics {
    pub count: u64,
    pub duration: LocalUsageReportDuration,
    pub duration_basis: String,
    pub outcomes: Vec<LocalUsageReportToolOutcome>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportToolOutcome {
    pub outcome: String,
    pub count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportTimeMetrics {
    pub request_to_delivery_wall: LocalUsageReportDuration,
    pub execution_wall_union: LocalUsageReportDuration,
    pub summed_per_agent_active: LocalUsageReportDuration,
    pub phase_interval_unions: Vec<LocalUsageReportNamedDuration>,
    pub activity_state_interval_unions: Vec<LocalUsageReportNamedDuration>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportNamedDuration {
    pub name: String,
    pub duration: LocalUsageReportDuration,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportClassificationCount {
    pub phase: String,
    pub activity: String,
    pub provenance: String,
    pub count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportRepositoryParticipation {
    pub operation_count: u64,
    pub tool_count: u64,
    pub additive: bool,
    pub label: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageReportFormulas {
    pub wall_time: String,
    pub tokens: String,
    pub concurrency: String,
    pub repository: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageSummaryParams {
    #[ts(optional = nullable)]
    pub repository_key: Option<String>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub account_id: Option<String>,
    #[ts(optional = nullable)]
    pub from_at: Option<i64>,
    #[ts(optional = nullable)]
    pub to_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageSummaryResponse {
    pub aggregate: LocalUsageAggregate,
    pub token_categories: Vec<LocalUsageTokenCategory>,
    pub report: LocalUsageReport,
    pub generated_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageThread {
    pub thread_id: String,
    pub repository_keys: Vec<String>,
    pub account_id: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
    pub aggregate: LocalUsageAggregate,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageThreadReadParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageThreadReadResponse {
    pub thread: LocalUsageThread,
    pub token_categories: Vec<LocalUsageTokenCategory>,
    pub report: LocalUsageReport,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepository {
    pub repository_key: String,
    pub label: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub aggregate: LocalUsageAggregate,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryListResponse {
    pub data: Vec<LocalUsageRepository>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryReadParams {
    pub repository_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryReadResponse {
    pub repository: LocalUsageRepository,
    pub token_categories: Vec<LocalUsageTokenCategory>,
    pub report: LocalUsageReport,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryUpdateParams {
    pub repository_key: String,
    pub label: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryUpdateResponse {
    pub repository: LocalUsageRepository,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryMergeParams {
    pub source_repository_key: String,
    pub target_repository_key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageRepositoryMergeResponse {
    pub repository: LocalUsageRepository,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageToolStatus {
    Completed,
    Failed,
    Interrupted,
    Rejected,
    Unsupported,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageTool {
    pub tool_call_id: String,
    pub thread_id: String,
    pub repository_key: Option<String>,
    pub tool_name: String,
    pub operation_family: String,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub status: LocalUsageToolStatus,
    pub provenance: LocalUsageProvenance,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageToolListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub repository_key: Option<String>,
    #[ts(optional = nullable)]
    pub from_at: Option<i64>,
    #[ts(optional = nullable)]
    pub to_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageToolListResponse {
    pub data: Vec<LocalUsageTool>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageActivity {
    pub activity_id: String,
    pub thread_id: String,
    pub agent_id: String,
    pub phase: LocalUsagePhase,
    pub activity: LocalUsageActivityKind,
    pub state: LocalUsageActivityState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub provenance: LocalUsageProvenance,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageActivityListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub agent_id: Option<String>,
    #[ts(optional = nullable)]
    pub from_at: Option<i64>,
    #[ts(optional = nullable)]
    pub to_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageActivityListResponse {
    pub data: Vec<LocalUsageActivity>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageEventKind {
    ModelRequestStarted,
    ModelRequestCompleted,
    ToolStarted,
    ToolCompleted,
    ActivityChanged,
    ClassificationCorrected,
    CoverageGap,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageEvent {
    pub event_id: String,
    pub thread_id: Option<String>,
    pub repository_key: Option<String>,
    pub occurred_at: i64,
    pub kind: LocalUsageEventKind,
    pub provenance: LocalUsageProvenance,
    pub coverage: LocalUsageCoverage,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageEventListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub repository_key: Option<String>,
    #[ts(optional = nullable)]
    pub kind: Option<LocalUsageEventKind>,
    #[ts(optional = nullable)]
    pub from_at: Option<i64>,
    #[ts(optional = nullable)]
    pub to_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageEventListResponse {
    pub data: Vec<LocalUsageEvent>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageClassificationCorrectParams {
    pub event_id: String,
    pub phase: LocalUsagePhase,
    pub activity: LocalUsageActivityKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageClassificationCorrectResponse {
    pub event: LocalUsageEvent,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum LocalUsageExportFormat {
    Json,
    Jsonl,
    Csv,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageExportCreateParams {
    pub format: LocalUsageExportFormat,
    /// Absolute caller-selected destination whose parent is already private.
    pub output_path: String,
    #[ts(optional = nullable)]
    pub repository_key: Option<String>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub from_at: Option<i64>,
    #[ts(optional = nullable)]
    pub to_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageExportCreateResponse {
    pub export_id: String,
    pub created_at: i64,
    pub file_name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct LocalUsageUpdatedNotification {
    pub generation: u64,
    pub updated_at: i64,
    pub thread_id: Option<String>,
    pub repository_key: Option<String>,
}
