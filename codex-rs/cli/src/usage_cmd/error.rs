use codex_usage::UsageStoreError;
use serde::Serialize;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsageErrorKind {
    Input,
    NotFound,
    Conflict,
    Configuration,
    Storage,
    Corrupt,
    Export,
}

impl UsageErrorKind {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Input => "invalid_input",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Configuration => "configuration",
            Self::Storage => "storage_unavailable",
            Self::Corrupt => "storage_corrupt",
            Self::Export => "export_failed",
        }
    }

    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Input => 2,
            Self::NotFound => 3,
            Self::Conflict => 4,
            Self::Configuration => 5,
            Self::Storage => 6,
            Self::Corrupt => 7,
            Self::Export => 8,
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Input => "usage command input is invalid or unsupported",
            Self::NotFound => "the requested usage record was not found",
            Self::Conflict => "the requested usage correction conflicts with existing history",
            Self::Configuration => "usage command configuration is invalid",
            Self::Storage => "usage storage is unavailable",
            Self::Corrupt => "usage storage failed integrity validation",
            Self::Export => "usage export could not be written privately and atomically",
        }
    }
}

#[derive(Debug)]
pub(crate) struct UsageCommandError {
    kind: UsageErrorKind,
}

impl UsageCommandError {
    pub(crate) const fn new(kind: UsageErrorKind) -> Self {
        Self { kind }
    }

    pub(crate) const fn kind(&self) -> UsageErrorKind {
        self.kind
    }

    pub(crate) const fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }
}

impl fmt::Display for UsageCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind.message())
    }
}

impl std::error::Error for UsageCommandError {}

impl From<UsageStoreError> for UsageCommandError {
    fn from(error: UsageStoreError) -> Self {
        let kind = match error {
            UsageStoreError::OperationConflict
            | UsageStoreError::ProcessConflict
            | UsageStoreError::ProcessEventConflict
            | UsageStoreError::TerminalConflict
            | UsageStoreError::FactConflict
            | UsageStoreError::RepositoryMergeCycle => UsageErrorKind::Conflict,
            UsageStoreError::RepositoryKeyMissing
            | UsageStoreError::RepositoryKeyCorrupt
            | UsageStoreError::Migration(_)
            | UsageStoreError::InvalidFact => UsageErrorKind::Corrupt,
            UsageStoreError::Database(_)
            | UsageStoreError::DurationOutOfRange
            | UsageStoreError::TokenCountOutOfRange
            | UsageStoreError::DatabaseValueOutOfRange
            | UsageStoreError::Filesystem(_)
            | UsageStoreError::UnsupportedPlatform
            | UsageStoreError::RepositoryKeyCommittedCleanupUncertain(_)
            | UsageStoreError::RepositoryKeyCommittedSyncUncertain(_)
            | UsageStoreError::AggregateOverflow
            | UsageStoreError::TaskTreeTooLarge => UsageErrorKind::Storage,
        };
        Self::new(kind)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope<'a> {
    schema_version: u32,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

pub(crate) fn print_error(error: &UsageCommandError, json: bool) {
    if json {
        let envelope = ErrorEnvelope {
            schema_version: 1,
            error: ErrorBody {
                code: error.kind().code(),
                message: error.kind().message(),
            },
        };
        eprintln!(
            "{}",
            serde_json::to_string(&envelope).unwrap_or_else(|_| {
                "{\"schemaVersion\":1,\"error\":{\"code\":\"storage_unavailable\",\"message\":\"usage storage is unavailable\"}}".to_string()
            })
        );
    } else {
        eprintln!("error: {error}");
    }
}
