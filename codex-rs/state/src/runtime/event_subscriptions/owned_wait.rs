use super::SqliteEventSubscriptionStore;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::MAX_TOTAL_SUBSCRIPTIONS;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::StoreError;
use codex_event_subscriptions::Subscription;
use std::ops::Deref;
use std::sync::PoisonError;
use tokio::sync::oneshot;
use uuid::Uuid;

pub(super) struct WaitRequest {
    subscription: Option<NewSubscription>,
    now_ms: i64,
    response: Option<oneshot::Sender<Result<Subscription, StoreError>>>,
    created_id: Option<Uuid>,
    cancelled: bool,
}

struct WaitRegistration {
    id: Uuid,
    store: SqliteEventSubscriptionStore,
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        let mut requests = self
            .store
            .wait_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if requests
            .get(&self.id)
            .is_some_and(|request| request.subscription.is_some())
        {
            requests.remove(&self.id);
        } else if let Some(request) = requests.get_mut(&self.id) {
            request.cancelled = true;
        }
        drop(requests);
        self.store.project_changed.notify_one();
    }
}

/// A native wait whose creation and dropped-handler cleanup belong to the subscription scheduler.
pub struct OwnedSubscription {
    subscription: Subscription,
    registration: WaitRegistration,
}

impl Deref for OwnedSubscription {
    type Target = Subscription;

    fn deref(&self) -> &Self::Target {
        &self.subscription
    }
}

impl OwnedSubscription {
    pub async fn cancel(self) -> Result<bool, StoreError> {
        let cancelled = self.registration.store.cancel(self.subscription.id).await?;
        self.registration
            .store
            .wait_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.registration.id);
        Ok(cancelled)
    }
}

impl SqliteEventSubscriptionStore {
    pub async fn create_wait(
        &self,
        subscription: NewSubscription,
        now_ms: i64,
    ) -> Result<OwnedSubscription, StoreError> {
        subscription
            .validate(now_ms)
            .map_err(|_| StoreError::InvalidData)?;
        let (response, receiver) = oneshot::channel();
        let id = Uuid::now_v7();
        {
            let mut requests = self
                .wait_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if requests.len() >= MAX_TOTAL_SUBSCRIPTIONS {
                return Err(StoreError::TotalCapacity);
            }
            requests.insert(
                id,
                WaitRequest {
                    subscription: Some(subscription),
                    now_ms,
                    response: Some(response),
                    created_id: None,
                    cancelled: false,
                },
            );
        }
        let registration = WaitRegistration {
            id,
            store: self.clone(),
        };
        self.project_changed.notify_one();
        let subscription = tokio::time::timeout(std::time::Duration::from_secs(5),receiver)
            .await
            .map_err(|_| StoreError::Unavailable("event wait scheduler did not accept the wait; use a running persistent app-server".into()))?
            .map_err(|_| StoreError::Unavailable("event wait owner stopped".into()))??;
        Ok(OwnedSubscription {
            subscription,
            registration,
        })
    }

    pub(super) async fn process_owned_wait_requests(&self) -> Result<(), StoreError> {
        let pending: Vec<_> = self
            .wait_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter_mut()
            .filter_map(|(id, request)| {
                if request.cancelled {
                    return None;
                }
                request
                    .subscription
                    .take()
                    .map(|subscription| (*id, subscription, request.now_ms))
            })
            .collect();
        for (id, subscription, now_ms) in pending {
            let result = self.create(subscription, now_ms).await;
            let mut requests = self
                .wait_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(mut request) = requests.remove(&id) {
                match result {
                    Ok(subscription) => {
                        request.created_id = Some(subscription.id);
                        if let Some(response) = request.response.take()
                            && response.send(Ok(subscription)).is_err()
                        {
                            request.cancelled = true;
                        }
                        requests.insert(id, request);
                    }
                    Err(error) => {
                        if let Some(response) = request.response.take() {
                            let _ = response.send(Err(error));
                        }
                    }
                }
            }
        }
        let cancelled: Vec<_> = self
            .wait_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, request)| request.cancelled)
            .map(|(id, request)| (*id, request.created_id))
            .collect();
        let mut failure = None;
        for (id, created_id) in cancelled {
            if let Some(created_id) = created_id
                && let Err(error) = self.cancel(created_id).await
            {
                failure = Some(error);
                continue;
            }
            self.wait_requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "owned_wait_tests.rs"]
mod tests;
