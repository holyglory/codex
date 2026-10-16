//! The same bounded, content-free report failure is used by native consumers.
use crate::ReportCacheStatus;
use crate::UsageStoreError;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReportFailure {
    pub code: &'static str,
    pub message: &'static str,
    pub retryable: bool,
    pub retry_after_ms: u32,
    pub cache: Option<ReportCacheStatus>,
}

impl UsageStoreError {
    pub fn report_failure(&self) -> Option<UsageReportFailure> {
        let (code, message, cache) = match self {
            Self::ReportWarming(cache) => (
                "usage_report_warming",
                "Usage summaries are warming. Retry or provide a finite time window.",
                Some((**cache).clone()),
            ),
            Self::ReportBusy => (
                "usage_report_busy",
                "Usage reporting is busy. Retry shortly.",
                None,
            ),
            Self::ReportTimedOut => (
                "usage_report_timeout",
                "Usage report exceeded its time budget. Narrow the scope or time window.",
                None,
            ),
            _ => return None,
        };
        Some(UsageReportFailure {
            code,
            message,
            cache,
            retryable: true,
            retry_after_ms: 1_000,
        })
    }
}

pub(crate) fn sqlite_code(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::Database(database) => match database
            .code()
            .and_then(|code| code.parse::<u32>().ok())
            .map(|code| code & 255)
        {
            Some(5) => "busy",
            Some(6) => "locked",
            Some(9) => "interrupt",
            Some(10) => "io",
            Some(11) => "corrupt",
            Some(13) => "full",
            Some(19) => "constraint",
            _ => "database",
        },
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::PoolClosed => "pool_closed",
        _ => "storage",
    }
}
