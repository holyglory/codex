use thiserror::Error;

use crate::UsageTerminalOutcome;

/// Error returned while executing a model-visible tool invocation.
#[derive(Debug, Error, PartialEq)]
pub enum FunctionCallError {
    #[error("{0}")]
    RespondToModel(String),
    #[error("Fatal error: {0}")]
    Fatal(String),
    #[error("{message}")]
    UsageClassified {
        message: String,
        outcome: UsageTerminalOutcome,
    },
}

impl FunctionCallError {
    pub fn timed_out(message: impl Into<String>) -> Self {
        Self::UsageClassified {
            message: message.into(),
            outcome: UsageTerminalOutcome::TIMED_OUT,
        }
    }

    pub fn usage_terminal_outcome(&self) -> Option<UsageTerminalOutcome> {
        match self {
            Self::UsageClassified { outcome, .. } => Some(*outcome),
            Self::RespondToModel(_) | Self::Fatal(_) => None,
        }
    }
}
