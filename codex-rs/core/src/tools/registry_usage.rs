use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use crate::usage_runtime::RepositoryCandidate;
use crate::usage_runtime::ToolAttemptContext;
use crate::usage_runtime::UsageToolDescriptor;
use crate::usage_runtime::repository_safe_label;
use crate::usage_runtime::usage_account_snapshot;
use codex_tools::ToolName;
use codex_usage::ToolExecutionGroupId;
use codex_usage::ToolExecutionRole;
use std::sync::Arc;

pub(crate) fn usage_tool_descriptor(
    name: &ToolName,
    runtime: Option<&dyn CoreToolRuntime>,
) -> UsageToolDescriptor {
    let descriptor = |kind, safe_name, family| UsageToolDescriptor {
        kind,
        safe_name,
        family,
        activity_control: false,
        activity_state: codex_usage::ActivityState::ToolActive,
    };
    if name.is_default_namespace() {
        if name.name.starts_with("browser") || name.name.starts_with("playwright") {
            return descriptor("browser", "browser_tool", "browser_automation");
        }
        if name.name.starts_with("extension_") {
            return descriptor("plugin", "plugin_tool", "extension");
        }
        return match name.name.as_str() {
            "usage_activity" => UsageToolDescriptor {
                kind: "control",
                safe_name: "usage_activity",
                family: "accounting",
                activity_control: true,
                activity_state: codex_usage::ActivityState::ToolActive,
            },
            "account_management" => descriptor("builtin", "account_management", "configuration"),
            "usage_stats" => UsageToolDescriptor {
                kind: "control",
                safe_name: "usage_stats",
                family: "accounting",
                activity_control: true,
                activity_state: codex_usage::ActivityState::ToolActive,
            },
            "shell_command" | "exec_command" | "write_stdin" => {
                descriptor("shell", "shell", "execution")
            }
            "apply_patch" => descriptor("builtin", "apply_patch", "coding"),
            "view_image" | "imagegen" => descriptor("image", "image", "media"),
            "tool_search" => descriptor("builtin", "tool_search", "discovery"),
            "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource" => {
                descriptor("mcp", "mcp_resource", "external_context")
            }
            "wait_agent" => UsageToolDescriptor {
                activity_state: codex_usage::ActivityState::ExternalWait,
                ..descriptor("collaboration", "collaboration", "coordination")
            },
            "spawn_agent" | "send_input" | "close_agent" | "resume_agent" | "list_agents"
            | "interrupt_agent" | "send_message" | "followup_task" => {
                descriptor("collaboration", "collaboration", "coordination")
            }
            "update_plan" => descriptor("builtin", "update_plan", "planning"),
            "request_user_input" => UsageToolDescriptor {
                activity_state: codex_usage::ActivityState::UserWait,
                ..descriptor("builtin", "request_user_input", "user_interaction")
            },
            "request_plugin_install" => {
                descriptor("plugin", "request_plugin_install", "configuration")
            }
            "request_permissions" => descriptor("builtin", "request_permissions", "permissions"),
            "sleep" | "test_sync" | "test_sync_tool" | "wait_for_environment" | "await_work" => {
                UsageToolDescriptor {
                    activity_state: codex_usage::ActivityState::ExternalWait,
                    ..descriptor("builtin", "utility", "control")
                }
            }
            "current_time"
            | "get_context_remaining"
            | "new_context_window"
            | "send_user_message_async" => descriptor("builtin", "utility", "control"),
            _ if runtime.is_some() => descriptor("dynamic", "dynamic_tool", "dynamic"),
            _ => descriptor("unsupported", "unsupported_tool", "unsupported"),
        };
    }
    if runtime.and_then(CoreToolRuntime::mcp_server_name).is_some() {
        descriptor("mcp", "mcp_tool", "external_context")
    } else if runtime.is_some() {
        descriptor("plugin", "plugin_tool", "extension")
    } else {
        descriptor("unsupported", "unsupported_tool", "unsupported")
    }
}

pub(crate) async fn begin_tool_attempt(
    invocation: &ToolInvocation,
    runtime: &Arc<dyn CoreToolRuntime>,
) -> crate::usage_runtime::UsageToolAttempt {
    let turn = &invocation.turn;
    let account = usage_account_snapshot(
        turn.account_lease
            .as_ref()
            .map(|lease| lease.account_id().as_str()),
        turn.account_lease
            .as_ref()
            .and_then(|lease| lease.auth_manager().get_api_auth_mode())
            .or_else(|| {
                turn.auth_manager
                    .as_ref()
                    .and_then(|manager| manager.get_api_auth_mode())
            }),
    );
    let thread_id = invocation.session.thread_id().to_string();
    let parent_thread_id = turn.parent_thread_id.map(|id| id.to_string());
    let repositories = repository_candidates(invocation);
    let (execution_group_id, execution_role) = execution_group(invocation).await;
    invocation
        .session
        .services
        .usage_runtime
        .begin_tool_attempt(ToolAttemptContext {
            thread_id: &thread_id,
            parent_thread_id: parent_thread_id.as_deref(),
            turn_id: Some(turn.sub_id.as_str()),
            delegated: turn.session_source.is_non_root_agent(),
            call_id: &invocation.call_id,
            cancellation_token: invocation.cancellation_token.clone(),
            descriptor: usage_tool_descriptor(&invocation.tool_name, Some(runtime.as_ref())),
            execution_group_id,
            execution_role,
            account,
            repositories,
        })
        .await
}

fn repository_candidates(invocation: &ToolInvocation) -> Vec<RepositoryCandidate> {
    let Some(environment) = invocation.step_context.environments.primary() else {
        return Vec::new();
    };
    let workspace = environment.cwd().to_path_buf().display().to_string();
    vec![RepositoryCandidate::new(
        workspace.clone(),
        None,
        repository_safe_label(&workspace),
    )]
}

async fn execution_group(
    invocation: &ToolInvocation,
) -> (Option<ToolExecutionGroupId>, ToolExecutionRole) {
    let role = match &invocation.source {
        ToolCallSource::CodeMode { .. } => ToolExecutionRole::Nested,
        ToolCallSource::Direct
            if invocation.tool_name.is_default_namespace()
                && matches!(
                    invocation.tool_name.name.as_str(),
                    crate::tools::code_mode::PUBLIC_TOOL_NAME
                        | crate::tools::code_mode::WAIT_TOOL_NAME
                ) =>
        {
            ToolExecutionRole::Wrapper
        }
        ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => {
            ToolExecutionRole::Standalone
        }
    };
    if role == ToolExecutionRole::Standalone {
        return (None, role);
    }
    let group = invocation
        .originating_call()
        .await
        .and_then(|origin| origin.item_id)
        .map(|item_id| {
            let key = format!(
                "{}\0{}\0{}",
                invocation.session.thread_id,
                invocation.turn.sub_id,
                item_id.as_str()
            );
            ToolExecutionGroupId::from_stable_key(key.as_bytes())
        });
    (group, role)
}

pub(crate) fn usage_terminal_status(
    outcome: codex_tools::UsageTerminalOutcome,
) -> (
    codex_usage::TerminalStatus,
    Option<codex_usage::ErrorCategory>,
) {
    let status = match outcome.status {
        codex_tools::UsageTerminalStatus::Completed => codex_usage::TerminalStatus::Completed,
        codex_tools::UsageTerminalStatus::Failed => codex_usage::TerminalStatus::Failed,
        codex_tools::UsageTerminalStatus::Denied => codex_usage::TerminalStatus::Denied,
        codex_tools::UsageTerminalStatus::TimedOut => codex_usage::TerminalStatus::TimedOut,
        codex_tools::UsageTerminalStatus::Cancelled => codex_usage::TerminalStatus::Cancelled,
    };
    let error = outcome.error_category.map(|category| match category {
        codex_tools::UsageTerminalErrorCategory::Tool => codex_usage::ErrorCategory::Tool,
        codex_tools::UsageTerminalErrorCategory::Timeout => codex_usage::ErrorCategory::Timeout,
        codex_tools::UsageTerminalErrorCategory::Cancelled => codex_usage::ErrorCategory::Cancelled,
        codex_tools::UsageTerminalErrorCategory::Provider => codex_usage::ErrorCategory::Provider,
    });
    (status, error)
}
