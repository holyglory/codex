use codex_app_server_protocol as api;
use codex_event_subscriptions as native;
use native::EventSubscriptionStore;

use super::EventSubscriptionRequestProcessor;
use super::internal_error;
use super::invalid_params;
use super::invalid_request;
use super::parse_subscription_id;
use super::parse_thread_id;

const MAX_POLICY_RESPONSE_BYTES: usize = 8 * 1024;

impl EventSubscriptionRequestProcessor {
    pub(crate) async fn wake_policy(
        &self,
        params: api::EventSubscriptionWakePolicyParams,
    ) -> Result<api::EventSubscriptionWakePolicyResponse, api::JSONRPCErrorError> {
        self.service()?;
        let thread_id = parse_thread_id(&params.thread_id)?;
        self.require_persistent_thread(thread_id).await?;
        let offset = params
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| invalid_params("invalid wake-policy cursor"))?;
        let limit = params.limit.unwrap_or(8) as usize;
        if limit == 0
            || limit > 16
            || (offset != 0
                && matches!(
                    &params.command,
                    Some(api::EventWakePolicyCommand::Set { .. })
                ))
        {
            return Err(invalid_params("invalid wake-policy page"));
        }
        let store = self
            .state_db
            .as_ref()
            .ok_or_else(|| invalid_request("wake policies require persistent state"))?
            .event_subscriptions();
        let run = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .ok()
            .map(|thread| thread.subscription_run_state());
        let _dispatch = if matches!(
            &params.command,
            Some(api::EventWakePolicyCommand::Set { .. })
        ) {
            match &run {
                Some(run) => Some(run.dispatch.lock().await),
                None => None,
            }
        } else {
            None
        };
        let mut changed_scope = None;
        let snapshot = match params.command.unwrap_or(api::EventWakePolicyCommand::Read) {
            api::EventWakePolicyCommand::Read => store.read_wake_policy(thread_id).await,
            api::EventWakePolicyCommand::Set {
                scope,
                policy,
                expected_revision,
                authorization_ref,
            } => {
                let scope = match scope {
                    api::EventWakeScope::Thread => native::WakeScope::Thread,
                    api::EventWakeScope::Subscription { subscription_id } => {
                        native::WakeScope::Subscription {
                            subscription_id: parse_subscription_id(&subscription_id)?,
                        }
                    }
                    api::EventWakeScope::ProjectDelivery { project_id, target } => {
                        native::WakeScope::ProjectDelivery { project_id, target }
                    }
                    api::EventWakeScope::ProjectReview { project_id } => {
                        native::WakeScope::ProjectReview { project_id }
                    }
                };
                let policy = match policy {
                    api::EventWakePolicy::RunningOnly => native::WakePolicy::RunningOnly,
                    api::EventWakePolicy::AllowBackground => native::WakePolicy::AllowBackground,
                };
                changed_scope = Some(scope.clone());
                store
                    .set_wake_policy(native::WakePolicyChange {
                        thread_id,
                        scope,
                        policy,
                        expected_revision,
                        authorization_ref,
                    })
                    .await
            }
        }
        .map_err(|error| invalid_request(error.to_string()))?;
        if offset > snapshot.policies.len() {
            return Err(invalid_params("invalid wake-policy cursor"));
        }
        let end = offset.saturating_add(limit).min(snapshot.policies.len());
        let next_cursor =
            (changed_scope.is_none() && end < snapshot.policies.len()).then(|| end.to_string());
        let entries = match &changed_scope {
            Some(scope) => snapshot
                .policies
                .iter()
                .filter(|entry| &entry.scope == scope)
                .collect::<Vec<_>>(),
            None => snapshot.policies[offset..end].iter().collect(),
        };
        let data = entries
            .into_iter()
            .map(|entry| {
                let scope = match &entry.scope {
                    native::WakeScope::Thread => api::EventWakeScope::Thread,
                    native::WakeScope::Subscription { subscription_id } => {
                        api::EventWakeScope::Subscription {
                            subscription_id: subscription_id.to_string(),
                        }
                    }
                    native::WakeScope::ProjectDelivery { project_id, target } => {
                        api::EventWakeScope::ProjectDelivery {
                            project_id: project_id.clone(),
                            target: target.clone(),
                        }
                    }
                    native::WakeScope::ProjectReview { project_id } => {
                        api::EventWakeScope::ProjectReview {
                            project_id: project_id.clone(),
                        }
                    }
                };
                api::EventWakePolicyEntry {
                    scope,
                    policy: match entry.policy {
                        native::WakePolicy::RunningOnly => api::EventWakePolicy::RunningOnly,
                        native::WakePolicy::AllowBackground => {
                            api::EventWakePolicy::AllowBackground
                        }
                    },
                    suspended: snapshot.stopped_revision > snapshot.resumed_revision
                        && entry.revision <= snapshot.stopped_revision,
                    authorization_ref: entry.authorization_ref.clone(),
                }
            })
            .collect();
        let running = match self.thread_manager.get_thread(thread_id).await {
            Ok(thread) => thread.has_running_user_work().await,
            Err(_) => false,
        };
        let pending_alarm_count = store
            .pending_wake(thread_id)
            .await
            .map_err(|_| internal_error("cannot read pending alarms"))?
            .map_or(0, |pending| pending.wake.items.len() as u32);
        let mut response = api::EventSubscriptionWakePolicyResponse {
            revision: snapshot.revision,
            running,
            pending_alarm_count,
            data,
            next_cursor,
        };
        while serde_json::to_string_pretty(&response)
            .map_err(|_| internal_error("cannot encode wake policies"))?
            .len()
            > MAX_POLICY_RESPONSE_BYTES
        {
            if response.data.len() <= 1 {
                return Err(internal_error(
                    "wake policy entry exceeds the response limit",
                ));
            }
            response.data.pop();
            response.next_cursor = Some((offset + response.data.len()).to_string());
        }
        Ok(response)
    }
}
