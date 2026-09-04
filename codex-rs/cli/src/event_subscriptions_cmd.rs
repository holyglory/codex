use std::collections::BTreeMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use codex_app_server_protocol::EventPublishParams;
use codex_app_server_protocol::EventSourceCursor;
use codex_app_server_protocol::EventSubscriptionCancelParams;
use codex_app_server_protocol::EventSubscriptionCreateParams;
use codex_app_server_protocol::EventSubscriptionFilter;
use codex_app_server_protocol::EventSubscriptionHeartbeat;
use codex_app_server_protocol::EventSubscriptionListParams;
use codex_app_server_protocol::EventSubscriptionTriggerParams;
use codex_app_server_protocol::IngressEvent;
use codex_arg0::Arg0DispatchPaths;
use codex_tui::Cli as TuiCli;
use codex_tui::EventSubscriptionAction;
use codex_utils_cli::CliConfigOverrides;
use uuid::Uuid;

use crate::InteractiveRemoteOptions;
use crate::SessionArchiveConfigOverrides;
use crate::finalize_session_archive_interactive;
use crate::resolve_remote_endpoint;

#[derive(Debug, Parser)]
pub(crate) struct EventSubscriptionsCommand {
    #[command(subcommand)]
    action: EventSubscriptionsSubcommand,

    /// Print the full structured response as JSON.
    #[arg(long)]
    json: bool,

    #[clap(flatten)]
    remote: InteractiveRemoteOptions,

    #[clap(flatten)]
    config_overrides: SessionArchiveConfigOverrides,
}

#[derive(Debug, Subcommand)]
enum EventSubscriptionsSubcommand {
    /// Create an event, heartbeat, or combined subscription for a thread.
    Create(CreateArgs),
    /// List durable subscriptions.
    List(ListArgs),
    /// Cancel one subscription.
    Cancel(CancelArgs),
    /// Trigger one or more subscriptions directly.
    Trigger(TriggerArgs),
    /// Publish one typed provider-neutral event.
    Publish(PublishArgs),
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Target thread UUID.
    #[arg(long)]
    thread: String,
    /// Provider-neutral event source name.
    #[arg(long)]
    source: Option<String>,
    /// Matching event type; repeat for multiple values.
    #[arg(long = "event-type")]
    event_types: Vec<String>,
    /// Required matching label in KEY=VALUE form; repeat for multiple values.
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Initial monotonically increasing source sequence.
    #[arg(long)]
    cursor_sequence: Option<u64>,
    /// Initial opaque source cursor value.
    #[arg(long, requires = "cursor_sequence")]
    cursor: Option<String>,
    /// Heartbeat interval in seconds.
    #[arg(long)]
    heartbeat_seconds: Option<u64>,
    /// Optional first heartbeat deadline as Unix seconds.
    #[arg(long, requires = "heartbeat_seconds")]
    first_heartbeat_at: Option<i64>,
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Limit results to one thread UUID.
    #[arg(long)]
    thread: Option<String>,
    /// Opaque cursor returned by a previous list call.
    #[arg(long)]
    cursor: Option<String>,
    /// Maximum number of subscriptions to return.
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Debug, Args)]
struct CancelArgs {
    /// Subscription UUID.
    #[arg(long)]
    id: String,
}

#[derive(Debug, Args)]
struct TriggerArgs {
    /// Subscription UUID; repeat to coalesce several into one thread continuation.
    #[arg(long = "id", required = true)]
    ids: Vec<String>,
}

#[derive(Debug, Args)]
struct PublishArgs {
    /// Event identifier used for diagnostics.
    #[arg(long)]
    id: Option<String>,
    /// Provider-neutral event source name.
    #[arg(long)]
    source: String,
    /// Provider-neutral event type.
    #[arg(long = "event-type")]
    event_type: String,
    /// Monotonically increasing source sequence used for deduplication.
    #[arg(long)]
    sequence: u64,
    /// Opaque source cursor value.
    #[arg(long)]
    cursor: Option<String>,
    /// Typed event label in KEY=VALUE form; repeat for multiple values.
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Event occurrence time as Unix seconds; defaults to the current clock.
    #[arg(long)]
    occurred_at: Option<i64>,
}

pub(crate) async fn run_event_subscriptions_command(
    command: EventSubscriptionsCommand,
    interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    root_remote: Option<String>,
    root_remote_auth_token_env: Option<String>,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<String> {
    let EventSubscriptionsCommand {
        action,
        json,
        remote,
        config_overrides,
    } = command;
    let cli =
        finalize_session_archive_interactive(interactive, root_config_overrides, config_overrides);
    if !cli.images.is_empty() {
        anyhow::bail!("`codex subscriptions` does not support image attachments");
    }
    let explicit_remote_endpoint = resolve_remote_endpoint(
        remote.remote.or(root_remote),
        remote.remote_auth_token_env.or(root_remote_auth_token_env),
    )?;
    let action = match action {
        EventSubscriptionsSubcommand::Create(args) => {
            EventSubscriptionAction::Create(EventSubscriptionCreateParams {
                thread_id: args.thread,
                filter: create_filter(args.source, args.event_types, args.labels)?,
                source_cursor: args.cursor_sequence.map(|sequence| EventSourceCursor {
                    sequence,
                    value: args.cursor,
                }),
                heartbeat: args.heartbeat_seconds.map(|interval_seconds| {
                    EventSubscriptionHeartbeat {
                        interval_seconds,
                        first_deadline_at: args.first_heartbeat_at,
                    }
                }),
            })
        }
        EventSubscriptionsSubcommand::List(args) => {
            EventSubscriptionAction::List(EventSubscriptionListParams {
                thread_id: args.thread,
                cursor: args.cursor,
                limit: args.limit,
            })
        }
        EventSubscriptionsSubcommand::Cancel(args) => {
            EventSubscriptionAction::Cancel(EventSubscriptionCancelParams {
                subscription_id: args.id,
            })
        }
        EventSubscriptionsSubcommand::Trigger(args) => {
            EventSubscriptionAction::Trigger(EventSubscriptionTriggerParams {
                subscription_ids: args.ids,
            })
        }
        EventSubscriptionsSubcommand::Publish(args) => {
            EventSubscriptionAction::Publish(EventPublishParams {
                event: IngressEvent {
                    id: args.id.unwrap_or_else(|| Uuid::now_v7().to_string()),
                    source: args.source,
                    event_type: args.event_type,
                    cursor: EventSourceCursor {
                        sequence: args.sequence,
                        value: args.cursor,
                    },
                    labels: parse_labels(args.labels)?,
                    occurred_at: args.occurred_at.unwrap_or_else(current_unix_seconds),
                },
            })
        }
    };
    codex_tui::run_event_subscription_command(
        action,
        json,
        codex_tui::SessionArchiveCommandOptions {
            cli,
            arg0_paths,
            explicit_remote_endpoint,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error:#}"))
}

fn create_filter(
    source: Option<String>,
    event_types: Vec<String>,
    labels: Vec<String>,
) -> anyhow::Result<Option<EventSubscriptionFilter>> {
    match (source, event_types.is_empty(), labels.is_empty()) {
        (None, true, true) => Ok(None),
        (Some(source), false, _) => Ok(Some(EventSubscriptionFilter {
            source,
            event_types,
            labels: parse_labels(labels)?,
        })),
        (None, _, _) => anyhow::bail!("--source is required when event filters are provided"),
        (Some(_), true, _) => {
            anyhow::bail!("at least one --event-type is required when --source is provided")
        }
    }
}

fn parse_labels(values: Vec<String>) -> anyhow::Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    for value in values {
        let Some((key, value)) = value.split_once('=') else {
            anyhow::bail!("event labels must use KEY=VALUE syntax");
        };
        if labels.insert(key.to_string(), value.to_string()).is_some() {
            anyhow::bail!("event label `{key}` was provided more than once");
        }
    }
    Ok(labels)
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "event_subscriptions_cmd_tests.rs"]
mod tests;
