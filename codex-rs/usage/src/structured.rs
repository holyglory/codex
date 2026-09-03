use crate::AccountProfileRef;
use crate::DurationAggregate;
use crate::UsageSummary;
use crate::UsageSummaryScope;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

pub const USAGE_REPORT_SCHEMA_VERSION: u32 = 1;

pub fn redacted_account_profile_label(account: &AccountProfileRef) -> String {
    let digest = Sha256::digest(account.as_str().as_bytes());
    let fingerprint = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("removed-account-{fingerprint}")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredUsageSummary {
    pub schema_version: u32,
    pub kind: &'static str,
    pub database_schema_version: u64,
    pub taxonomy_version: i64,
    pub scope: StructuredUsageScope,
    pub account: Option<String>,
    pub time_range: Option<StructuredTimeRange>,
    pub coverage: StructuredCoverage,
    pub counts: StructuredCounts,
    pub provider_tokens: Vec<StructuredTokenAggregate>,
    pub provider_tokens_by_activity: Vec<StructuredActivityTokenAggregate>,
    pub tools: StructuredToolMetrics,
    pub time: StructuredTimeMetrics,
    pub classifications: Vec<StructuredClassificationCount>,
    pub repository_participation: StructuredRepositoryParticipation,
    pub formulas: StructuredUsageFormulas,
}

impl StructuredUsageSummary {
    pub fn new(summary: &UsageSummary, account: Option<String>) -> Self {
        Self {
            schema_version: USAGE_REPORT_SCHEMA_VERSION,
            kind: "usageSummary",
            database_schema_version: summary.database_schema_version,
            taxonomy_version: summary.taxonomy_version,
            scope: match &summary.scope {
                UsageSummaryScope::All => StructuredUsageScope {
                    kind: "all",
                    id: None,
                },
                UsageSummaryScope::Thread(id) => StructuredUsageScope {
                    kind: "thread",
                    id: Some(id.as_str().to_string()),
                },
                UsageSummaryScope::Repository(id) => StructuredUsageScope {
                    kind: "repository",
                    id: Some(id.as_str().to_string()),
                },
            },
            account,
            time_range: summary.time_range.map(|range| StructuredTimeRange {
                start_ms: range.start_ms(),
                end_ms: range.end_ms(),
            }),
            coverage: StructuredCoverage {
                state: summary.coverage.overall_state.clone(),
                has_gaps: summary.coverage.has_gaps,
                events: summary
                    .coverage
                    .event_counts
                    .iter()
                    .map(|count| StructuredCoverageCount {
                        state: count.state.clone(),
                        count: count.count,
                    })
                    .collect(),
                token_observations: summary
                    .coverage
                    .token_observation_counts
                    .iter()
                    .map(|count| StructuredCoverageCount {
                        state: count.state.clone(),
                        count: count.count,
                    })
                    .collect(),
            },
            counts: StructuredCounts {
                operations: summary.operation_count,
                model_requests: summary.model_request_count,
                tools: summary.tool_count,
            },
            provider_tokens: summary
                .tokens
                .iter()
                .map(|tokens| StructuredTokenAggregate {
                    category: tokens.category_path.clone(),
                    repository_bucket: tokens.repository_bucket.clone(),
                    measurement_provenance: tokens.measurement_provenance.clone(),
                    measured_tokens: tokens.measured_tokens,
                    exact_tokens: tokens.exact_tokens,
                    unknown_observations: tokens.unknown_observations,
                    observation_count: tokens.observation_count,
                })
                .collect(),
            provider_tokens_by_activity: summary
                .provider_tokens_by_activity
                .iter()
                .map(|tokens| StructuredActivityTokenAggregate {
                    phase: tokens.phase.clone(),
                    activity: tokens.activity.clone(),
                    attribution_provenance: tokens.attribution_provenance.clone(),
                    measured_tokens: tokens.measured_tokens,
                    exact_tokens: tokens.exact_tokens,
                    unknown_observations: tokens.unknown_observations,
                })
                .collect(),
            tools: StructuredToolMetrics {
                count: summary.tools.count,
                duration: duration(&summary.tools.duration),
                duration_basis: summary.tools.duration_basis,
                outcomes: summary
                    .tools
                    .outcomes
                    .iter()
                    .map(|outcome| StructuredToolOutcome {
                        outcome: outcome.outcome.clone(),
                        count: outcome.count,
                    })
                    .collect(),
            },
            time: StructuredTimeMetrics {
                request_to_delivery_wall: duration(&summary.timing.request_to_delivery_wall),
                execution_wall_union: duration(&summary.timing.execution_wall_union),
                summed_per_agent_active: duration(&summary.timing.summed_per_agent_active),
                phase_interval_unions: summary
                    .timing
                    .phase_interval_unions
                    .iter()
                    .map(|item| StructuredNamedDuration {
                        name: item.name.clone(),
                        duration: duration(&item.duration),
                    })
                    .collect(),
                activity_state_interval_unions: summary
                    .timing
                    .activity_state_interval_unions
                    .iter()
                    .map(|item| StructuredNamedDuration {
                        name: item.name.clone(),
                        duration: duration(&item.duration),
                    })
                    .collect(),
            },
            classifications: summary
                .classifications
                .iter()
                .map(|classification| StructuredClassificationCount {
                    phase: classification.phase.clone(),
                    activity: classification.activity.clone(),
                    provenance: classification.provenance.clone(),
                    count: classification.count,
                })
                .collect(),
            repository_participation: StructuredRepositoryParticipation {
                operation_count: summary.repository_participation.operation_count,
                tool_count: summary.repository_participation.tool_count,
                additive: summary.repository_participation.additive,
                label: summary.repository_participation.label,
            },
            formulas: StructuredUsageFormulas {
                wall_time: "request_to_delivery_wall is an enclosing span; execution, phase, state, agent, and tool durations are reported separately and must not be summed",
                tokens: "provider-native categories are independent observations; provider_tokens_by_activity allocates provider-reported total_tokens only",
                concurrency: "interval unions deduplicate overlap; summed_per_agent_active intentionally includes concurrent agent time",
                repository: summary.aggregation,
            },
        }
    }
}

fn duration(value: &DurationAggregate) -> StructuredDuration {
    StructuredDuration {
        measured_ns: value.measured_ns,
        exact_ns: value.exact_ns,
        unknown_intervals: value.unknown_intervals,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredUsageScope {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredTimeRange {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredCoverage {
    pub state: String,
    pub has_gaps: bool,
    pub events: Vec<StructuredCoverageCount>,
    pub token_observations: Vec<StructuredCoverageCount>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredCoverageCount {
    pub state: String,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredCounts {
    pub operations: u64,
    pub model_requests: u64,
    pub tools: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredTokenAggregate {
    pub category: String,
    pub repository_bucket: String,
    pub measurement_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
    pub observation_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredActivityTokenAggregate {
    pub phase: String,
    pub activity: String,
    pub attribution_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredDuration {
    pub measured_ns: u64,
    pub exact_ns: Option<u64>,
    pub unknown_intervals: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredToolMetrics {
    pub count: u64,
    pub duration: StructuredDuration,
    pub duration_basis: &'static str,
    pub outcomes: Vec<StructuredToolOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredToolOutcome {
    pub outcome: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredTimeMetrics {
    pub request_to_delivery_wall: StructuredDuration,
    pub execution_wall_union: StructuredDuration,
    pub summed_per_agent_active: StructuredDuration,
    pub phase_interval_unions: Vec<StructuredNamedDuration>,
    pub activity_state_interval_unions: Vec<StructuredNamedDuration>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredNamedDuration {
    pub name: String,
    pub duration: StructuredDuration,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredClassificationCount {
    pub phase: String,
    pub activity: String,
    pub provenance: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredRepositoryParticipation {
    pub operation_count: u64,
    pub tool_count: u64,
    pub additive: bool,
    pub label: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredUsageFormulas {
    pub wall_time: &'static str,
    pub tokens: &'static str,
    pub concurrency: &'static str,
    pub repository: &'static str,
}
