use crate::report_math::ParticipationCounts;
use crate::report_math::ReportTimeMetrics;
use crate::report_math::TokenActivityAggregate;
use crate::report_math::ToolMetrics;
use crate::report_math::UtcTimeRange;
use crate::repository::RepositoryId;
use crate::store::UsageStore;
use crate::store::UsageStoreError;
use crate::types::AccountProfileRef;
use crate::types::ThreadId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageSummaryScope {
    All,
    Thread(ThreadId),
    Repository(RepositoryId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSummaryQuery {
    pub thread_id: Option<ThreadId>,
    pub repository_id: Option<RepositoryId>,
    pub account_profile_ref: Option<AccountProfileRef>,
    pub time_range: Option<UtcTimeRange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageCount {
    pub state: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageSummary {
    pub overall_state: String,
    pub event_counts: Vec<CoverageCount>,
    pub token_observation_counts: Vec<CoverageCount>,
    pub has_gaps: bool,
    pub unfinished_operations: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenAggregate {
    pub category_path: String,
    pub repository_bucket: String,
    pub measurement_provenance: String,
    pub measured_tokens: i64,
    pub exact_tokens: Option<i64>,
    pub unknown_observations: u64,
    pub observation_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassificationCount {
    pub phase: String,
    pub activity: String,
    pub provenance: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSummary {
    pub database_schema_version: u64,
    pub taxonomy_version: i64,
    pub scope: UsageSummaryScope,
    pub time_range: Option<UtcTimeRange>,
    pub coverage: CoverageSummary,
    pub tokens: Vec<TokenAggregate>,
    pub provider_tokens_by_activity: Vec<TokenActivityAggregate>,
    pub timing: ReportTimeMetrics,
    pub tools: ToolMetrics,
    pub repository_participation: ParticipationCounts,
    pub operation_count: u64,
    pub model_request_count: u64,
    pub tool_count: u64,
    pub classifications: Vec<ClassificationCount>,
    pub aggregation: &'static str,
}

impl UsageStore {
    pub async fn usage_summary(
        &self,
        scope: UsageSummaryScope,
    ) -> Result<UsageSummary, UsageStoreError> {
        self.usage_summary_in_range(scope, /*time_range*/ None)
            .await
    }

    pub async fn usage_summary_in_range(
        &self,
        scope: UsageSummaryScope,
        time_range: Option<UtcTimeRange>,
    ) -> Result<UsageSummary, UsageStoreError> {
        let query = match scope {
            UsageSummaryScope::All => UsageSummaryQuery {
                thread_id: None,
                repository_id: None,
                account_profile_ref: None,
                time_range,
            },
            UsageSummaryScope::Thread(thread_id) => UsageSummaryQuery {
                thread_id: Some(thread_id),
                repository_id: None,
                account_profile_ref: None,
                time_range,
            },
            UsageSummaryScope::Repository(repository_id) => UsageSummaryQuery {
                thread_id: None,
                repository_id: Some(repository_id),
                account_profile_ref: None,
                time_range,
            },
        };
        self.usage_summary_query(query).await
    }

    pub async fn usage_summary_query(
        &self,
        query: UsageSummaryQuery,
    ) -> Result<UsageSummary, UsageStoreError> {
        self.bounded_usage_summary(query).await
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
