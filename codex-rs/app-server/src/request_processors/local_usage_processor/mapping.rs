use codex_app_server_protocol as protocol;
use codex_app_server_protocol::LocalUsageActivity;
use codex_app_server_protocol::LocalUsageActivityKind;
use codex_app_server_protocol::LocalUsageActivityState;
use codex_app_server_protocol::LocalUsageAggregate;
use codex_app_server_protocol::LocalUsageCoverage;
use codex_app_server_protocol::LocalUsageEvent;
use codex_app_server_protocol::LocalUsageEventKind;
use codex_app_server_protocol::LocalUsagePhase;
use codex_app_server_protocol::LocalUsageProvenance;
use codex_app_server_protocol::LocalUsageReport;
use codex_app_server_protocol::LocalUsageTokenCategory;
use codex_app_server_protocol::LocalUsageTool;
use codex_app_server_protocol::LocalUsageToolStatus;
use codex_usage::Activity;
use codex_usage::ActivityState;
use codex_usage::AttributionProvenance;
use codex_usage::CoverageState;
use codex_usage::Phase;
use codex_usage::StructuredActivityTokenAggregate;
use codex_usage::StructuredClassificationCount;
use codex_usage::StructuredCoverage;
use codex_usage::StructuredCoverageCount;
use codex_usage::StructuredDuration;
use codex_usage::StructuredNamedDuration;
use codex_usage::StructuredRepositoryParticipation;
use codex_usage::StructuredTimeMetrics;
use codex_usage::StructuredTimeRange;
use codex_usage::StructuredTokenAggregate;
use codex_usage::StructuredToolMetrics;
use codex_usage::StructuredToolOutcome;
use codex_usage::StructuredUsageFormulas;
use codex_usage::StructuredUsageScope;
use codex_usage::StructuredUsageSummary;
use codex_usage::UsageActivityRecord;
use codex_usage::UsageEventKind;
use codex_usage::UsageEventProvenance;
use codex_usage::UsageEventRecord;
use codex_usage::UsageStoreError;
use codex_usage::UsageSummary;
use codex_usage::UsageToolRecord;
use std::collections::BTreeMap;

pub(super) fn summary(
    summary: &UsageSummary,
) -> Result<(LocalUsageAggregate, Vec<LocalUsageTokenCategory>), UsageStoreError> {
    let mut grouped = BTreeMap::<String, CategoryAccumulator>::new();
    for token in &summary.tokens {
        let aggregate = grouped.entry(token.category_path.clone()).or_default();
        aggregate.measured = aggregate
            .measured
            .checked_add(token.measured_tokens)
            .ok_or(UsageStoreError::AggregateOverflow)?;
        aggregate.exact = match (aggregate.exact, token.exact_tokens) {
            (Some(left), Some(right)) => left.checked_add(right),
            _ => None,
        };
        aggregate.provenance = merge_provenance(
            aggregate.provenance,
            measurement_provenance(&token.measurement_provenance)?,
        );
        aggregate.observed = true;
    }
    let token_categories = grouped
        .iter()
        .map(|(category_key, aggregate)| LocalUsageTokenCategory {
            category_key: category_key.clone(),
            count: aggregate.exact,
            provenance: aggregate
                .provenance
                .unwrap_or(LocalUsageProvenance::Unknown),
            coverage: category_coverage(aggregate),
        })
        .collect::<Vec<_>>();
    let known = |path: &str| grouped.get(path).and_then(|aggregate| aggregate.exact);
    let duration_ms = i64::try_from(
        summary
            .timing
            .execution_wall_union
            .measured_ns
            .checked_div(1_000_000)
            .ok_or(UsageStoreError::AggregateOverflow)?,
    )
    .map_err(|_| UsageStoreError::AggregateOverflow)?;
    Ok((
        LocalUsageAggregate {
            input_tokens: known("input_tokens"),
            cached_input_tokens: known("input_tokens_details.cached_tokens"),
            cache_write_input_tokens: known("input_tokens_details.cache_write_tokens"),
            output_tokens: known("output_tokens"),
            reasoning_output_tokens: known("output_tokens_details.reasoning_tokens"),
            total_tokens: known("total_tokens"),
            model_requests: i64::try_from(summary.model_request_count)
                .map_err(|_| UsageStoreError::AggregateOverflow)?,
            tool_calls: i64::try_from(summary.tool_count)
                .map_err(|_| UsageStoreError::AggregateOverflow)?,
            duration_ms,
            coverage: summary_coverage(summary),
        },
        token_categories,
    ))
}

pub(super) fn report(summary: &UsageSummary, account: Option<String>) -> LocalUsageReport {
    let StructuredUsageSummary {
        schema_version,
        kind,
        database_schema_version,
        taxonomy_version,
        scope,
        account,
        time_range,
        coverage,
        counts,
        provider_tokens,
        provider_tokens_by_activity,
        tools,
        time,
        classifications,
        repository_participation,
        formulas,
    } = StructuredUsageSummary::new(summary, account);
    LocalUsageReport {
        schema_version,
        kind: kind.to_string(),
        database_schema_version,
        taxonomy_version,
        scope: report_scope(scope),
        account,
        time_range: time_range.map(report_time_range),
        coverage: report_coverage(coverage),
        counts: protocol::LocalUsageReportCounts {
            operations: counts.operations,
            model_requests: counts.model_requests,
            tools: counts.tools,
        },
        provider_tokens: provider_tokens
            .into_iter()
            .map(report_token_aggregate)
            .collect(),
        provider_tokens_by_activity: provider_tokens_by_activity
            .into_iter()
            .map(report_activity_token_aggregate)
            .collect(),
        tools: report_tool_metrics(tools),
        time: report_time_metrics(time),
        classifications: classifications
            .into_iter()
            .map(report_classification)
            .collect(),
        repository_participation: report_repository_participation(repository_participation),
        formulas: report_formulas(formulas),
    }
}

fn report_scope(value: StructuredUsageScope) -> protocol::LocalUsageReportScope {
    protocol::LocalUsageReportScope {
        kind: value.kind.to_string(),
        id: value.id,
    }
}

fn report_time_range(value: StructuredTimeRange) -> protocol::LocalUsageReportTimeRange {
    protocol::LocalUsageReportTimeRange {
        start_ms: value.start_ms,
        end_ms: value.end_ms,
    }
}

fn report_coverage(value: StructuredCoverage) -> protocol::LocalUsageReportCoverage {
    protocol::LocalUsageReportCoverage {
        state: value.state,
        has_gaps: value.has_gaps,
        events: value
            .events
            .into_iter()
            .map(report_coverage_count)
            .collect(),
        token_observations: value
            .token_observations
            .into_iter()
            .map(report_coverage_count)
            .collect(),
    }
}

fn report_coverage_count(
    value: StructuredCoverageCount,
) -> protocol::LocalUsageReportCoverageCount {
    protocol::LocalUsageReportCoverageCount {
        state: value.state,
        count: value.count,
    }
}

fn report_token_aggregate(
    value: StructuredTokenAggregate,
) -> protocol::LocalUsageReportTokenAggregate {
    protocol::LocalUsageReportTokenAggregate {
        category: value.category,
        repository_bucket: value.repository_bucket,
        repository_label: None,
        measurement_provenance: value.measurement_provenance,
        measured_tokens: value.measured_tokens,
        exact_tokens: value.exact_tokens,
        unknown_observations: value.unknown_observations,
        observation_count: value.observation_count,
    }
}

fn report_activity_token_aggregate(
    value: StructuredActivityTokenAggregate,
) -> protocol::LocalUsageReportActivityTokenAggregate {
    protocol::LocalUsageReportActivityTokenAggregate {
        phase: value.phase,
        activity: value.activity,
        attribution_provenance: value.attribution_provenance,
        measured_tokens: value.measured_tokens,
        exact_tokens: value.exact_tokens,
        unknown_observations: value.unknown_observations,
    }
}

fn report_duration(value: StructuredDuration) -> protocol::LocalUsageReportDuration {
    protocol::LocalUsageReportDuration {
        measured_ns: value.measured_ns,
        exact_ns: value.exact_ns,
        unknown_intervals: value.unknown_intervals,
    }
}

fn report_tool_metrics(value: StructuredToolMetrics) -> protocol::LocalUsageReportToolMetrics {
    protocol::LocalUsageReportToolMetrics {
        count: value.count,
        duration: report_duration(value.duration),
        duration_basis: value.duration_basis.to_string(),
        outcomes: value
            .outcomes
            .into_iter()
            .map(report_tool_outcome)
            .collect(),
    }
}

fn report_tool_outcome(value: StructuredToolOutcome) -> protocol::LocalUsageReportToolOutcome {
    protocol::LocalUsageReportToolOutcome {
        outcome: value.outcome,
        count: value.count,
    }
}

fn report_time_metrics(value: StructuredTimeMetrics) -> protocol::LocalUsageReportTimeMetrics {
    protocol::LocalUsageReportTimeMetrics {
        request_to_delivery_wall: report_duration(value.request_to_delivery_wall),
        execution_wall_union: report_duration(value.execution_wall_union),
        summed_per_agent_active: report_duration(value.summed_per_agent_active),
        phase_interval_unions: value
            .phase_interval_unions
            .into_iter()
            .map(report_named_duration)
            .collect(),
        activity_state_interval_unions: value
            .activity_state_interval_unions
            .into_iter()
            .map(report_named_duration)
            .collect(),
    }
}

fn report_named_duration(
    value: StructuredNamedDuration,
) -> protocol::LocalUsageReportNamedDuration {
    protocol::LocalUsageReportNamedDuration {
        name: value.name,
        duration: report_duration(value.duration),
    }
}

fn report_classification(
    value: StructuredClassificationCount,
) -> protocol::LocalUsageReportClassificationCount {
    protocol::LocalUsageReportClassificationCount {
        phase: value.phase,
        activity: value.activity,
        provenance: value.provenance,
        count: value.count,
    }
}

fn report_repository_participation(
    value: StructuredRepositoryParticipation,
) -> protocol::LocalUsageReportRepositoryParticipation {
    protocol::LocalUsageReportRepositoryParticipation {
        operation_count: value.operation_count,
        tool_count: value.tool_count,
        additive: value.additive,
        label: value.label.to_string(),
    }
}

fn report_formulas(value: StructuredUsageFormulas) -> protocol::LocalUsageReportFormulas {
    protocol::LocalUsageReportFormulas {
        wall_time: value.wall_time.to_string(),
        tokens: value.tokens.to_string(),
        concurrency: value.concurrency.to_string(),
        repository: value.repository.to_string(),
    }
}

pub(super) fn tool(record: UsageToolRecord) -> LocalUsageTool {
    LocalUsageTool {
        tool_call_id: record.id.as_string(),
        thread_id: record.thread_id.as_str().to_string(),
        repository_key: record.repository_id.map(|id| id.as_str().to_string()),
        tool_name: record.tool_name.as_str().to_string(),
        operation_family: record.operation_family.as_str().to_string(),
        started_at: seconds(record.started_at_ms),
        completed_at: record.completed_at_ms.map(seconds),
        status: match record.status {
            Some(codex_usage::TerminalStatus::Completed) => LocalUsageToolStatus::Completed,
            Some(codex_usage::TerminalStatus::Failed) => LocalUsageToolStatus::Failed,
            Some(codex_usage::TerminalStatus::Denied) => LocalUsageToolStatus::Rejected,
            Some(codex_usage::TerminalStatus::TimedOut)
            | Some(codex_usage::TerminalStatus::Cancelled)
            | Some(codex_usage::TerminalStatus::Interrupted) => LocalUsageToolStatus::Interrupted,
            Some(codex_usage::TerminalStatus::Incomplete) | None => LocalUsageToolStatus::Unknown,
        },
        provenance: attribution_provenance(record.provenance),
    }
}

pub(super) fn activity(record: UsageActivityRecord) -> LocalUsageActivity {
    LocalUsageActivity {
        activity_id: record.id.as_string(),
        thread_id: record.thread_id.as_str().to_string(),
        agent_id: record.agent_id.as_str().to_string(),
        phase: phase(record.phase),
        activity: activity_kind(record.activity),
        state: activity_state(record.state),
        started_at: seconds(record.started_at_ms),
        ended_at: record.ended_at_ms.map(seconds),
        provenance: attribution_provenance(record.provenance),
    }
}

pub(super) fn event(record: UsageEventRecord) -> LocalUsageEvent {
    LocalUsageEvent {
        event_id: record.id.as_string(),
        thread_id: record.thread_id.map(|id| id.as_str().to_string()),
        repository_key: record.repository_id.map(|id| id.as_str().to_string()),
        occurred_at: seconds(record.occurred_at_ms),
        kind: event_kind(record.kind),
        provenance: event_provenance(record.provenance),
        coverage: coverage(record.coverage),
    }
}

pub(super) const fn phase(value: Phase) -> LocalUsagePhase {
    match value {
        Phase::Planning => LocalUsagePhase::Planning,
        Phase::Implementation => LocalUsagePhase::Implementation,
        Phase::Testing => LocalUsagePhase::Testing,
        Phase::Deployment => LocalUsagePhase::Deployment,
        Phase::Reporting => LocalUsagePhase::Reporting,
        Phase::Unattributed => LocalUsagePhase::Unattributed,
    }
}

pub(super) const fn activity_kind(value: Activity) -> LocalUsageActivityKind {
    match value {
        Activity::Requirements => LocalUsageActivityKind::Requirements,
        Activity::Specification => LocalUsageActivityKind::Specification,
        Activity::RepositoryAnalysis => LocalUsageActivityKind::RepositoryAnalysis,
        Activity::Research => LocalUsageActivityKind::Research,
        Activity::Diagnosis => LocalUsageActivityKind::Diagnosis,
        Activity::ArchitectureDesign => LocalUsageActivityKind::ArchitectureDesign,
        Activity::WorkPlanning => LocalUsageActivityKind::WorkPlanning,
        Activity::Coding => LocalUsageActivityKind::Coding,
        Activity::Configuration => LocalUsageActivityKind::Configuration,
        Activity::Refactoring => LocalUsageActivityKind::Refactoring,
        Activity::DependencyOrBuildChange => LocalUsageActivityKind::DependencyOrBuildChange,
        Activity::TestAuthoring => LocalUsageActivityKind::TestAuthoring,
        Activity::DocumentationAuthoring => LocalUsageActivityKind::DocumentationAuthoring,
        Activity::DataOrSchemaChange => LocalUsageActivityKind::DataOrSchemaChange,
        Activity::BuildValidation => LocalUsageActivityKind::BuildValidation,
        Activity::UnitTesting => LocalUsageActivityKind::UnitTesting,
        Activity::IntegrationTesting => LocalUsageActivityKind::IntegrationTesting,
        Activity::BrowserQa => LocalUsageActivityKind::BrowserQa,
        Activity::CompatibilityTesting => LocalUsageActivityKind::CompatibilityTesting,
        Activity::MigrationRehearsal => LocalUsageActivityKind::MigrationRehearsal,
        Activity::VerificationReview => LocalUsageActivityKind::VerificationReview,
        Activity::Packaging => LocalUsageActivityKind::Packaging,
        Activity::Deployment => LocalUsageActivityKind::Deployment,
        Activity::Rollback => LocalUsageActivityKind::Rollback,
        Activity::RuntimeOperations => LocalUsageActivityKind::RuntimeOperations,
        Activity::Monitoring => LocalUsageActivityKind::Monitoring,
        Activity::UserElaboration => LocalUsageActivityKind::UserElaboration,
        Activity::StatusUpdate => LocalUsageActivityKind::StatusUpdate,
        Activity::CompletionHandoff => LocalUsageActivityKind::CompletionHandoff,
        Activity::ReviewFeedback => LocalUsageActivityKind::ReviewFeedback,
        Activity::Coordination => LocalUsageActivityKind::Coordination,
        Activity::AccountingOverhead => LocalUsageActivityKind::AccountingOverhead,
        Activity::Mixed => LocalUsageActivityKind::Mixed,
        Activity::Unknown => LocalUsageActivityKind::Unknown,
    }
}

pub(super) const fn activity_state(value: ActivityState) -> LocalUsageActivityState {
    match value {
        ActivityState::ModelActive => LocalUsageActivityState::ModelActive,
        ActivityState::ToolActive => LocalUsageActivityState::ToolActive,
        ActivityState::ExternalWait => LocalUsageActivityState::ExternalWait,
        ActivityState::UserWait => LocalUsageActivityState::UserWait,
        ActivityState::BlockedWait => LocalUsageActivityState::BlockedWait,
    }
}

pub(super) const fn event_kind(value: UsageEventKind) -> LocalUsageEventKind {
    match value {
        UsageEventKind::ModelRequestStarted => LocalUsageEventKind::ModelRequestStarted,
        UsageEventKind::ModelRequestCompleted => LocalUsageEventKind::ModelRequestCompleted,
        UsageEventKind::ToolStarted => LocalUsageEventKind::ToolStarted,
        UsageEventKind::ToolCompleted => LocalUsageEventKind::ToolCompleted,
        UsageEventKind::ActivityChanged => LocalUsageEventKind::ActivityChanged,
        UsageEventKind::ClassificationCorrected => LocalUsageEventKind::ClassificationCorrected,
        UsageEventKind::CoverageGap => LocalUsageEventKind::CoverageGap,
    }
}

pub(super) const fn coverage(value: CoverageState) -> LocalUsageCoverage {
    match value {
        CoverageState::Complete => LocalUsageCoverage::Complete,
        CoverageState::CaptureStarted | CoverageState::Partial | CoverageState::Recovery => {
            LocalUsageCoverage::Partial
        }
        CoverageState::Unknown | CoverageState::Corrupt | CoverageState::Unavailable => {
            LocalUsageCoverage::Unknown
        }
    }
}

pub(super) fn seconds(milliseconds: i64) -> i64 {
    milliseconds.div_euclid(1_000)
}

fn summary_coverage(summary: &UsageSummary) -> LocalUsageCoverage {
    match summary.coverage.overall_state.as_str() {
        "complete" => LocalUsageCoverage::Complete,
        "unobserved" => LocalUsageCoverage::Unknown,
        _ if summary.operation_count > 0 || !summary.tokens.is_empty() => {
            LocalUsageCoverage::Partial
        }
        _ => LocalUsageCoverage::Unknown,
    }
}

struct CategoryAccumulator {
    measured: i64,
    exact: Option<i64>,
    provenance: Option<LocalUsageProvenance>,
    observed: bool,
}

impl Default for CategoryAccumulator {
    fn default() -> Self {
        Self {
            measured: 0,
            exact: Some(0),
            provenance: None,
            observed: false,
        }
    }
}

fn category_coverage(aggregate: &CategoryAccumulator) -> LocalUsageCoverage {
    if aggregate.exact.is_some() {
        LocalUsageCoverage::Complete
    } else if aggregate.observed {
        LocalUsageCoverage::Partial
    } else {
        LocalUsageCoverage::Unknown
    }
}

fn merge_provenance(
    current: Option<LocalUsageProvenance>,
    next: LocalUsageProvenance,
) -> Option<LocalUsageProvenance> {
    Some(match current {
        None => next,
        Some(current) if current == next => current,
        Some(_) => LocalUsageProvenance::Unknown,
    })
}

fn measurement_provenance(value: &str) -> Result<LocalUsageProvenance, UsageStoreError> {
    match value {
        "provider_reported" => Ok(LocalUsageProvenance::ProviderReported),
        "imported" => Ok(LocalUsageProvenance::Imported),
        "unknown" => Ok(LocalUsageProvenance::Unknown),
        _ => Err(UsageStoreError::InvalidFact),
    }
}

fn attribution_provenance(value: AttributionProvenance) -> LocalUsageProvenance {
    match value {
        AttributionProvenance::AgentDeclared => LocalUsageProvenance::AgentDeclared,
        AttributionProvenance::DeterministicClassification => {
            LocalUsageProvenance::DeterministicClassification
        }
        AttributionProvenance::InferredClassification => {
            LocalUsageProvenance::InferredClassification
        }
        AttributionProvenance::UserCorrected => LocalUsageProvenance::UserCorrected,
        AttributionProvenance::Unknown => LocalUsageProvenance::Unknown,
    }
}

fn event_provenance(value: UsageEventProvenance) -> LocalUsageProvenance {
    match value {
        UsageEventProvenance::ProviderReported => LocalUsageProvenance::ProviderReported,
        UsageEventProvenance::RuntimeObserved => LocalUsageProvenance::RuntimeObserved,
        UsageEventProvenance::AgentDeclared => LocalUsageProvenance::AgentDeclared,
        UsageEventProvenance::DeterministicClassification => {
            LocalUsageProvenance::DeterministicClassification
        }
        UsageEventProvenance::InferredClassification => {
            LocalUsageProvenance::InferredClassification
        }
        UsageEventProvenance::UserCorrected => LocalUsageProvenance::UserCorrected,
        UsageEventProvenance::Imported => LocalUsageProvenance::Imported,
        UsageEventProvenance::Unknown => LocalUsageProvenance::Unknown,
    }
}
