use std::sync::Arc;

use codex_app_server_protocol::EventPublishParams;
use codex_app_server_protocol::EventPublishResponse;
use codex_app_server_protocol::EventSourceCursor as ApiSourceCursor;
use codex_app_server_protocol::EventSubscription as ApiSubscription;
use codex_app_server_protocol::EventSubscriptionCancelParams;
use codex_app_server_protocol::EventSubscriptionCancelResponse;
use codex_app_server_protocol::EventSubscriptionCreateParams;
use codex_app_server_protocol::EventSubscriptionCreateResponse;
use codex_app_server_protocol::EventSubscriptionFilter as ApiFilter;
use codex_app_server_protocol::EventSubscriptionListParams;
use codex_app_server_protocol::EventSubscriptionListResponse;
use codex_app_server_protocol::EventSubscriptionTriggerParams;
use codex_app_server_protocol::EventSubscriptionTriggerResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::HeartbeatSpec;
use codex_event_subscriptions::ListSubscriptionsQuery;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::ServiceError;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::StoreError;
use codex_event_subscriptions::Subscription;
use codex_protocol::ThreadId;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStore;
use uuid::Uuid;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;

const DEFAULT_LIST_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct EventSubscriptionRequestProcessor {
    service: Option<EventSubscriptionService>,
    thread_store: Arc<dyn ThreadStore>,
    thread_manager: Arc<codex_core::ThreadManager>,
}

impl EventSubscriptionRequestProcessor {
    pub(crate) fn new(
        service: Option<EventSubscriptionService>,
        thread_store: Arc<dyn ThreadStore>,
        thread_manager: Arc<codex_core::ThreadManager>,
    ) -> Self {
        Self {
            service,
            thread_store,
            thread_manager,
        }
    }

    pub(crate) async fn create(
        &self,
        params: EventSubscriptionCreateParams,
    ) -> Result<EventSubscriptionCreateResponse, JSONRPCErrorError> {
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.require_persistent_thread(thread_id).await?;
        let filter = params.filter.map(|filter| EventFilter {
            source: filter.source,
            event_types: filter.event_types.into_iter().collect(),
            labels: filter.labels,
        });
        let source_cursor = params.source_cursor.map(source_cursor);
        let heartbeat = params
            .heartbeat
            .map(|heartbeat| {
                Ok(HeartbeatSpec {
                    interval_ms: seconds_to_millis(heartbeat.interval_seconds)?,
                    first_deadline_at_ms: heartbeat
                        .first_deadline_at
                        .map(seconds_timestamp_to_millis)
                        .transpose()?,
                })
            })
            .transpose()?;
        let subscription = self
            .service()?
            .create(NewSubscription {
                thread_id,
                filter,
                source_cursor,
                heartbeat,
            })
            .await
            .map_err(service_error)?;
        Ok(EventSubscriptionCreateResponse {
            subscription: api_subscription(subscription),
        })
    }

    pub(crate) async fn list(
        &self,
        params: EventSubscriptionListParams,
    ) -> Result<EventSubscriptionListResponse, JSONRPCErrorError> {
        let thread_id = params
            .thread_id
            .as_deref()
            .map(parse_thread_id)
            .transpose()?;
        let offset = params
            .cursor
            .as_deref()
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|error| invalid_params(format!("invalid pagination cursor: {error}")))?
            .unwrap_or_default();
        let limit = params
            .limit
            .map(|limit| limit as usize)
            .unwrap_or(DEFAULT_LIST_LIMIT);
        let page = self
            .service()?
            .list(ListSubscriptionsQuery {
                thread_id,
                offset,
                limit,
            })
            .await
            .map_err(service_error)?;
        Ok(EventSubscriptionListResponse {
            data: page.data.into_iter().map(api_subscription).collect(),
            next_cursor: page.next_offset.map(|offset| offset.to_string()),
        })
    }

    pub(crate) async fn cancel(
        &self,
        params: EventSubscriptionCancelParams,
    ) -> Result<EventSubscriptionCancelResponse, JSONRPCErrorError> {
        let subscription_id = parse_subscription_id(&params.subscription_id)?;
        let cancelled = self
            .service()?
            .cancel(subscription_id)
            .await
            .map_err(service_error)?;
        Ok(EventSubscriptionCancelResponse { cancelled })
    }

    pub(crate) async fn trigger(
        &self,
        params: EventSubscriptionTriggerParams,
    ) -> Result<EventSubscriptionTriggerResponse, JSONRPCErrorError> {
        let subscription_ids = params
            .subscription_ids
            .iter()
            .map(|id| parse_subscription_id(id))
            .collect::<Result<Vec<_>, _>>()?;
        let outcome = self
            .service()?
            .trigger(subscription_ids)
            .await
            .map_err(service_error)?;
        Ok(EventSubscriptionTriggerResponse {
            triggered_subscription_ids: outcome
                .triggered_subscription_ids
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
            missing_subscription_ids: outcome
                .missing_subscription_ids
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
        })
    }

    pub(crate) async fn publish(
        &self,
        params: EventPublishParams,
    ) -> Result<EventPublishResponse, JSONRPCErrorError> {
        let event = params.event;
        let outcome = self
            .service()?
            .publish(PublishedEvent {
                id: event.id,
                source: event.source,
                event_type: event.event_type,
                cursor: source_cursor(event.cursor),
                labels: event.labels,
                occurred_at_ms: seconds_timestamp_to_millis(event.occurred_at)?,
            })
            .await
            .map_err(service_error)?;
        Ok(EventPublishResponse {
            accepted_subscription_ids: outcome
                .accepted_subscription_ids
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
            ignored_subscription_ids: outcome
                .ignored_subscription_ids
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
        })
    }

    fn service(&self) -> Result<&EventSubscriptionService, JSONRPCErrorError> {
        self.service.as_ref().ok_or_else(|| {
            invalid_request(
                "event subscriptions are unavailable; enable `event_subscriptions` with the durable local thread store",
            )
        })
    }

    async fn require_persistent_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), JSONRPCErrorError> {
        if let Ok(thread) = self.thread_manager.get_thread(thread_id).await {
            if thread.config_snapshot().await.ephemeral {
                return Err(invalid_request(format!(
                    "ephemeral thread does not support event subscriptions: {thread_id}"
                )));
            }
            return Ok(());
        }
        let thread = self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|error| invalid_request(format!("thread not found: {thread_id}: {error}")))?;
        if thread.archived_at.is_some() {
            return Err(invalid_request(format!(
                "archived thread does not support new event subscriptions: {thread_id}"
            )));
        }
        Ok(())
    }
}

fn api_subscription(subscription: Subscription) -> ApiSubscription {
    ApiSubscription {
        id: subscription.id.to_string(),
        thread_id: subscription.thread_id.to_string(),
        filter: subscription.filter.map(|filter| ApiFilter {
            source: filter.source,
            event_types: filter.event_types.into_iter().collect(),
            labels: filter.labels,
        }),
        source_cursor: subscription.source_cursor.map(api_source_cursor),
        heartbeat_interval_seconds: subscription
            .heartbeat_interval_ms
            .and_then(|interval| u64::try_from(interval / 1_000).ok()),
        next_heartbeat_at: subscription.next_heartbeat_at_ms.map(millis_to_seconds),
        created_at: millis_to_seconds(subscription.created_at_ms),
        updated_at: millis_to_seconds(subscription.updated_at_ms),
    }
}

fn source_cursor(cursor: ApiSourceCursor) -> SourceCursor {
    SourceCursor {
        sequence: cursor.sequence,
        value: cursor.value,
    }
}

fn api_source_cursor(cursor: SourceCursor) -> ApiSourceCursor {
    ApiSourceCursor {
        sequence: cursor.sequence,
        value: cursor.value,
    }
}

fn parse_thread_id(value: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(value)
        .map_err(|error| invalid_params(format!("invalid thread id: {error}")))
}

fn parse_subscription_id(value: &str) -> Result<Uuid, JSONRPCErrorError> {
    Uuid::parse_str(value)
        .map_err(|error| invalid_params(format!("invalid subscription id: {error}")))
}

fn seconds_to_millis(seconds: u64) -> Result<i64, JSONRPCErrorError> {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .ok_or_else(|| invalid_params("heartbeat interval is outside the supported range"))
}

fn seconds_timestamp_to_millis(seconds: i64) -> Result<i64, JSONRPCErrorError> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| invalid_params("timestamp is outside the supported range"))
}

fn millis_to_seconds(milliseconds: i64) -> i64 {
    milliseconds.div_euclid(1_000)
}

fn service_error(error: ServiceError) -> JSONRPCErrorError {
    match error {
        ServiceError::Validation(error) => invalid_params(error.to_string()),
        ServiceError::Store(StoreError::TotalCapacity | StoreError::ThreadCapacity) => {
            invalid_request(error.to_string())
        }
        ServiceError::Store(StoreError::InvalidData) => {
            internal_error("stored event subscription data is invalid")
        }
        ServiceError::Store(StoreError::Unavailable(_)) => {
            internal_error("event subscription storage is unavailable")
        }
        ServiceError::SchedulerUnavailable => {
            internal_error("event subscription scheduler is unavailable")
        }
    }
}
