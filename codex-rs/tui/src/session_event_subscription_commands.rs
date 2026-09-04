//! Manage provider-neutral event subscriptions through a local or remote app server.

use crate::session_archive_commands::SessionArchiveCommandOptions;
use crate::session_archive_commands::start_app_server_for_session_command;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::EventPublishParams;
use codex_app_server_protocol::EventPublishResponse;
use codex_app_server_protocol::EventSubscriptionCancelParams;
use codex_app_server_protocol::EventSubscriptionCancelResponse;
use codex_app_server_protocol::EventSubscriptionCreateParams;
use codex_app_server_protocol::EventSubscriptionCreateResponse;
use codex_app_server_protocol::EventSubscriptionListParams;
use codex_app_server_protocol::EventSubscriptionListResponse;
use codex_app_server_protocol::EventSubscriptionTriggerParams;
use codex_app_server_protocol::EventSubscriptionTriggerResponse;
use codex_utils_home_dir::find_codex_home;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::eyre;

pub enum EventSubscriptionAction {
    Create(EventSubscriptionCreateParams),
    List(EventSubscriptionListParams),
    Cancel(EventSubscriptionCancelParams),
    Trigger(EventSubscriptionTriggerParams),
    Publish(EventPublishParams),
}

pub async fn run_event_subscription_command(
    action: EventSubscriptionAction,
    json: bool,
    options: SessionArchiveCommandOptions,
) -> Result<String> {
    let codex_home = find_codex_home().wrap_err("failed to find Codex home")?;
    let explicit_remote = options.explicit_remote_endpoint.is_some();
    let mut app_server =
        start_app_server_for_session_command(options, codex_home.to_path_buf()).await?;
    if !explicit_remote && app_server.uses_embedded_app_server() {
        return Err(eyre!(
            "event subscriptions require the persistent local app-server daemon; enable the feature in config, run `codex app-server daemon start`, then retry (or use --remote)"
        ));
    }
    if !app_server.supports_event_subscriptions() {
        return Err(eyre!(
            "the app server does not advertise event-subscription capability version 1; enable the feature and restart that server"
        ));
    }

    match action {
        EventSubscriptionAction::Create(params) => {
            let request_id = app_server.next_request_id();
            let response: EventSubscriptionCreateResponse = app_server
                .request_handle()
                .request_typed(ClientRequest::EventSubscriptionCreate { request_id, params })
                .await
                .wrap_err("failed to create event subscription")?;
            if json {
                return pretty_json(&response);
            }
            Ok(format!(
                "Created subscription {} for thread {}.",
                response.subscription.id, response.subscription.thread_id
            ))
        }
        EventSubscriptionAction::List(params) => {
            let request_id = app_server.next_request_id();
            let response: EventSubscriptionListResponse = app_server
                .request_handle()
                .request_typed(ClientRequest::EventSubscriptionList { request_id, params })
                .await
                .wrap_err("failed to list event subscriptions")?;
            if json {
                return pretty_json(&response);
            }
            if response.data.is_empty() {
                return Ok("No event subscriptions found.".to_string());
            }
            Ok(response
                .data
                .into_iter()
                .map(|subscription| format!("{}\t{}", subscription.id, subscription.thread_id))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        EventSubscriptionAction::Cancel(params) => {
            let subscription_id = params.subscription_id.clone();
            let request_id = app_server.next_request_id();
            let response: EventSubscriptionCancelResponse = app_server
                .request_handle()
                .request_typed(ClientRequest::EventSubscriptionCancel { request_id, params })
                .await
                .wrap_err("failed to cancel event subscription")?;
            if json {
                return pretty_json(&response);
            }
            Ok(if response.cancelled {
                format!("Cancelled subscription {subscription_id}.")
            } else {
                format!("Subscription {subscription_id} was not found.")
            })
        }
        EventSubscriptionAction::Trigger(params) => {
            let request_id = app_server.next_request_id();
            let response: EventSubscriptionTriggerResponse = app_server
                .request_handle()
                .request_typed(ClientRequest::EventSubscriptionTrigger { request_id, params })
                .await
                .wrap_err("failed to trigger event subscriptions")?;
            if json {
                return pretty_json(&response);
            }
            Ok(format!(
                "Triggered {} subscription(s); {} not found.",
                response.triggered_subscription_ids.len(),
                response.missing_subscription_ids.len()
            ))
        }
        EventSubscriptionAction::Publish(params) => {
            let request_id = app_server.next_request_id();
            let response: EventPublishResponse = app_server
                .request_handle()
                .request_typed(ClientRequest::EventPublish { request_id, params })
                .await
                .wrap_err("failed to publish event")?;
            if json {
                return pretty_json(&response);
            }
            Ok(format!(
                "Accepted by {} subscription(s); ignored as duplicate or out of order by {}.",
                response.accepted_subscription_ids.len(),
                response.ignored_subscription_ids.len()
            ))
        }
    }
}

fn pretty_json(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(Into::into)
}
