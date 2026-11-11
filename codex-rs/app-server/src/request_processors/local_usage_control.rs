//! Explicit job declarations use the same capture queue as model and tool starts.

use std::sync::Arc;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LocalUsageBindingSnapshot;
use codex_app_server_protocol::LocalUsageBindingStatusParams;
use codex_app_server_protocol::LocalUsageBindingStatusResponse;
use codex_app_server_protocol::LocalUsageBindingTransitionParams;
use codex_app_server_protocol::LocalUsageBindingTransitionResponse;
use codex_core::ThreadManager;
use codex_core::UsageWorkAction;
use codex_protocol::ThreadId;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;

pub(crate) async fn binding_transition(
    manager: Arc<ThreadManager>,
    params: LocalUsageBindingTransitionParams,
) -> Result<LocalUsageBindingTransitionResponse, JSONRPCErrorError> {
    if params.version != 1 {
        return Err(invalid_params("unsupported local work binding version"));
    }
    let thread_id = ThreadId::from_string(&params.thread_id)
        .map_err(|_| invalid_params("local work binding requires a valid thread ID"))?;
    let action = match params.native_project_id {
        Some(native_project_id) => UsageWorkAction::Set {
            native_project_id,
            workstream_id: params.workstream_id,
            outcome_id: params.outcome_id,
            expected_revision: params.expected_revision,
        },
        None => {
            if params.workstream_id.is_some() || params.outcome_id.is_some() {
                return Err(invalid_params(
                    "clearing work cannot include workstream or outcome IDs",
                ));
            }
            UsageWorkAction::Clear {
                expected_revision: params.expected_revision,
            }
        }
    };
    let receipt = manager
        .usage_work(thread_id, action)
        .await
        .map_err(capture_error)?;
    let transition = receipt
        .transition
        .ok_or_else(|| internal_error("local work binding receipt unavailable; inspect status"))?;
    Ok(LocalUsageBindingTransitionResponse {
        version: 1,
        revision: receipt.revision,
        effective_capture_sequence: transition.effective_capture_sequence,
        captured_at_ms: transition.captured_at_ms,
    })
}

pub(crate) async fn binding_status(
    manager: Arc<ThreadManager>,
    params: LocalUsageBindingStatusParams,
) -> Result<LocalUsageBindingStatusResponse, JSONRPCErrorError> {
    if params.version.unwrap_or(/*default*/ 1) != 1 {
        return Err(invalid_params("unsupported local work binding version"));
    }
    let thread_id = ThreadId::from_string(&params.thread_id)
        .map_err(|_| invalid_params("local work binding requires a valid thread ID"))?;
    let receipt = manager
        .usage_work(thread_id, UsageWorkAction::Status)
        .await
        .map_err(capture_error)?;
    Ok(LocalUsageBindingStatusResponse {
        version: 1,
        revision: receipt.revision,
        transition: receipt
            .transition
            .map(|transition| LocalUsageBindingSnapshot {
                thread_id: transition.update.thread_id,
                native_project_id: transition.update.native_project_id,
                workstream_id: transition.update.workstream_id,
                outcome_id: transition.update.outcome_id,
                effective_capture_sequence: transition.effective_capture_sequence,
                captured_at_ms: transition.captured_at_ms,
                source: transition.update.source,
                authority: transition.update.authority,
            }),
    })
}

fn capture_error(message: String) -> JSONRPCErrorError {
    if message.starts_with("usage_work revision")
        || message.starts_with("usage_work identifiers")
        || message == "usage_work requires a loaded thread"
    {
        invalid_params(message)
    } else {
        internal_error(
            "local work binding capture unavailable or pending; inspect status before retrying",
        )
    }
}
