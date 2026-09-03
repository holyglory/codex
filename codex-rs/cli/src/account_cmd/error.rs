use serde::Serialize;

use super::JSON_SCHEMA_VERSION;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AccountErrorKind {
    UnknownAccount,
    AmbiguousAccount,
    DisabledAccount,
    NotAuthenticated,
    AccountInUse,
    GenerationConflict,
    ConfirmationRequired,
    LoginCancelled,
    DuplicateAccount,
    RateLimitsUnavailable,
    InvalidInput,
    CredentialStore,
    Registry,
    Configuration,
    Integrity,
    Output,
}

#[derive(Debug)]
pub(crate) struct AccountCommandError {
    kind: AccountErrorKind,
}

impl AccountCommandError {
    pub(super) fn new(kind: AccountErrorKind) -> Self {
        Self { kind }
    }

    pub(crate) fn exit_code(&self) -> i32 {
        match self.kind {
            AccountErrorKind::UnknownAccount => 10,
            AccountErrorKind::AmbiguousAccount => 11,
            AccountErrorKind::DisabledAccount => 12,
            AccountErrorKind::NotAuthenticated => 13,
            AccountErrorKind::AccountInUse => 14,
            AccountErrorKind::GenerationConflict => 15,
            AccountErrorKind::ConfirmationRequired | AccountErrorKind::InvalidInput => 16,
            AccountErrorKind::LoginCancelled => 17,
            AccountErrorKind::RateLimitsUnavailable => 18,
            AccountErrorKind::DuplicateAccount => 19,
            AccountErrorKind::CredentialStore => 20,
            AccountErrorKind::Registry => 21,
            AccountErrorKind::Configuration => 22,
            AccountErrorKind::Integrity => 23,
            AccountErrorKind::Output => 24,
        }
    }

    #[cfg(test)]
    pub(super) fn kind(&self) -> AccountErrorKind {
        self.kind
    }

    fn code(&self) -> &'static str {
        match self.kind {
            AccountErrorKind::UnknownAccount => "unknownAccount",
            AccountErrorKind::AmbiguousAccount => "ambiguousAccount",
            AccountErrorKind::DisabledAccount => "disabledAccount",
            AccountErrorKind::NotAuthenticated => "notAuthenticated",
            AccountErrorKind::AccountInUse => "accountInUse",
            AccountErrorKind::GenerationConflict => "generationConflict",
            AccountErrorKind::ConfirmationRequired => "confirmationRequired",
            AccountErrorKind::LoginCancelled => "loginCancelled",
            AccountErrorKind::DuplicateAccount => "duplicateAccount",
            AccountErrorKind::RateLimitsUnavailable => "rateLimitsUnavailable",
            AccountErrorKind::InvalidInput => "invalidInput",
            AccountErrorKind::CredentialStore => "credentialStoreFailure",
            AccountErrorKind::Registry => "registryFailure",
            AccountErrorKind::Configuration => "configurationFailure",
            AccountErrorKind::Integrity => "integrityFailure",
            AccountErrorKind::Output => "outputFailure",
        }
    }

    fn message(&self) -> &'static str {
        match self.kind {
            AccountErrorKind::UnknownAccount => "account profile was not found",
            AccountErrorKind::AmbiguousAccount => "account reference is ambiguous",
            AccountErrorKind::DisabledAccount => "account profile is disabled",
            AccountErrorKind::NotAuthenticated => "account profile is not authenticated",
            AccountErrorKind::AccountInUse => "account profile is in use",
            AccountErrorKind::GenerationConflict => "account registry changed concurrently",
            AccountErrorKind::ConfirmationRequired => {
                "explicit confirmation is required; pass --yes"
            }
            AccountErrorKind::LoginCancelled => "account login was cancelled",
            AccountErrorKind::DuplicateAccount => "account profile is already registered",
            AccountErrorKind::RateLimitsUnavailable => "rate-limit information is unavailable",
            AccountErrorKind::InvalidInput => "account command input is invalid",
            AccountErrorKind::CredentialStore => "credential storage operation failed",
            AccountErrorKind::Registry => "account registry operation failed",
            AccountErrorKind::Configuration => "Codex configuration could not be loaded",
            AccountErrorKind::Integrity => "account profile integrity checks failed",
            AccountErrorKind::Output => "account command output could not be encoded",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonError<'a> {
    schema_version: u32,
    error: JsonErrorBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

pub(crate) fn print_error(error: &AccountCommandError, json: bool) {
    if json {
        let payload = JsonError {
            schema_version: JSON_SCHEMA_VERSION,
            error: JsonErrorBody {
                code: error.code(),
                message: error.message(),
            },
        };
        eprintln!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| {
                "{\"schemaVersion\":1,\"error\":{\"code\":\"outputFailure\",\"message\":\"account command output could not be encoded\"}}".to_string()
            })
        );
    } else {
        eprintln!("Error: {}", error.message());
    }
}
