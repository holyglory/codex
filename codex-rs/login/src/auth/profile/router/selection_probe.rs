use std::collections::HashSet;
use std::future::Future;

use chrono::Utc;
use codex_account_registry::AccountId;
use codex_account_registry::AccountRegistry;
use codex_account_selection::AccountLimitCache;
use codex_account_selection::Eligibility;
use codex_account_selection::SelectionError;
use codex_account_selection::supports_automatic_selection;
use codex_protocol::protocol::RateLimitSnapshot;

use super::AccountLease;
use super::AuthManagerLease;
use super::MAX_LIMIT_AGE_SECONDS;
use super::ProfileAuthRouter;
use super::ProfileAuthRouterError;
use super::RouterExternalAuthState;
use super::SharedProfileAuthRouter;
use super::router_state_error;

struct ProbeSelectionState {
    registry: AccountRegistry,
    current_account_id: Option<AccountId>,
    authenticated_accounts: HashSet<AccountId>,
    cache: AccountLimitCache,
    now: i64,
}

#[derive(Clone, Copy)]
enum SelectionStateDisposition {
    Commit,
    Preview,
}

impl SharedProfileAuthRouter {
    /// Acquires a turn lease after explicitly probing unknown automatic-selection candidates.
    ///
    /// The probe is opt-in so callers without a reviewed, read-only availability check retain the
    /// fail-closed behavior of [`Self::lease_for_turn_with_external_auth`]. Probe failures leave
    /// capacity unknown and never make an account eligible implicitly.
    pub async fn lease_for_turn_with_external_auth_and_probe<Probe, ProbeFuture>(
        &self,
        external_auth: RouterExternalAuthState,
        probe: Probe,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError>
    where
        Probe: Fn(AuthManagerLease) -> ProbeFuture + Send + Sync,
        ProbeFuture: Future<Output = Option<Vec<RateLimitSnapshot>>> + Send,
    {
        let excluded_account_ids = HashSet::new();
        self.lease_with_external_auth_and_probe(
            external_auth,
            SelectionStateDisposition::Commit,
            &excluded_account_ids,
            /*require_auto_selection*/ false,
            probe,
        )
        .await
    }

    /// Acquires a startup-prewarm lease without consuming the first real turn's switch state.
    ///
    /// Availability observations are shared normally, but the selected account remains unchanged
    /// until a real turn commits it and can surface the automatic-switch notice.
    pub async fn lease_for_startup_prewarm_with_external_auth_and_probe<Probe, ProbeFuture>(
        &self,
        external_auth: RouterExternalAuthState,
        probe: Probe,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError>
    where
        Probe: Fn(AuthManagerLease) -> ProbeFuture + Send + Sync,
        ProbeFuture: Future<Output = Option<Vec<RateLimitSnapshot>>> + Send,
    {
        let excluded_account_ids = HashSet::new();
        self.lease_with_external_auth_and_probe(
            external_auth,
            SelectionStateDisposition::Preview,
            &excluded_account_ids,
            /*require_auto_selection*/ false,
            probe,
        )
        .await
    }

    /// Acquires a different automatically selected profile after a clean usage-limit rejection.
    ///
    /// Excluded profiles are never probed or selected. A process pin, disabled automatic
    /// selection, or a pool without another eligible profile returns `None` so the original
    /// provider error remains authoritative.
    pub async fn lease_for_usage_limit_failover_with_external_auth_and_probe<Probe, ProbeFuture>(
        &self,
        external_auth: RouterExternalAuthState,
        excluded_account_ids: &HashSet<AccountId>,
        probe: Probe,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError>
    where
        Probe: Fn(AuthManagerLease) -> ProbeFuture + Send + Sync,
        ProbeFuture: Future<Output = Option<Vec<RateLimitSnapshot>>> + Send,
    {
        if excluded_account_ids.is_empty() {
            return Ok(None);
        }
        self.lease_with_external_auth_and_probe(
            external_auth,
            SelectionStateDisposition::Commit,
            excluded_account_ids,
            /*require_auto_selection*/ true,
            probe,
        )
        .await
    }

    async fn lease_with_external_auth_and_probe<Probe, ProbeFuture>(
        &self,
        external_auth: RouterExternalAuthState,
        disposition: SelectionStateDisposition,
        excluded_account_ids: &HashSet<AccountId>,
        require_auto_selection: bool,
        probe: Probe,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError>
    where
        Probe: Fn(AuthManagerLease) -> ProbeFuture + Send + Sync,
        ProbeFuture: Future<Output = Option<Vec<RateLimitSnapshot>>> + Send,
    {
        let Some(router) = self
            .router_if_configured_with_external(external_auth)
            .await?
        else {
            return Ok(None);
        };
        if router.inner.process_pin.is_some() {
            if require_auto_selection {
                return Ok(None);
            }
            return router.lease_for_turn().await.map(Some);
        }

        router.reload_at_turn_boundary().await?;
        if !router.registry_snapshot()?.auto_selection.enabled {
            if require_auto_selection {
                return Ok(None);
            }
            return match disposition {
                SelectionStateDisposition::Commit => {
                    self.lease_for_turn_with_external_auth(external_auth).await
                }
                SelectionStateDisposition::Preview => {
                    let registry = router.registry_snapshot()?;
                    let account_id = registry
                        .default_account_id
                        .as_ref()
                        .ok_or(ProfileAuthRouterError::UnknownAccount)?;
                    router.lease(&registry, account_id).map(Some)
                }
            };
        }

        let initial = self
            .probe_selection_state(&router, excluded_account_ids)
            .await?;
        if next_probe_target(&initial, &HashSet::new()).is_none() {
            return self.finish_probed_selection_or_none(
                &router,
                initial,
                disposition,
                require_auto_selection,
            );
        }

        let probe_guard = self
            .inner
            .selection_probe_lock
            .acquire()
            .await
            .map_err(|_| router_state_error())?;
        let mut attempted = excluded_account_ids.clone();
        loop {
            let state = self
                .probe_selection_state(&router, excluded_account_ids)
                .await?;
            if !state.registry.auto_selection.enabled {
                drop(probe_guard);
                if require_auto_selection {
                    return Ok(None);
                }
                return match disposition {
                    SelectionStateDisposition::Commit => {
                        self.lease_for_turn_with_external_auth(external_auth).await
                    }
                    SelectionStateDisposition::Preview => {
                        let account_id = state
                            .registry
                            .default_account_id
                            .as_ref()
                            .ok_or(ProfileAuthRouterError::UnknownAccount)?;
                        router.lease(&state.registry, account_id).map(Some)
                    }
                };
            }
            let Some(account_id) = next_probe_target(&state, &attempted) else {
                return self.finish_probed_selection_or_none(
                    &router,
                    state,
                    disposition,
                    require_auto_selection,
                );
            };

            let lease = match router.lease_for_account(&account_id).await {
                Ok(lease) => lease,
                Err(
                    ProfileAuthRouterError::UnknownAccount
                    | ProfileAuthRouterError::DisabledAccount
                    | ProfileAuthRouterError::NotAuthenticated,
                ) => {
                    attempted.insert(account_id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            attempted.insert(account_id.clone());
            let probe_lease = AuthManagerLease::profile(lease);
            let Some(snapshots) = probe(probe_lease).await.filter(|items| !items.is_empty()) else {
                continue;
            };
            let _ = self.record_rate_limits(account_id, Utc::now().timestamp(), snapshots);
        }
    }

    async fn probe_selection_state(
        &self,
        router: &ProfileAuthRouter,
        excluded_account_ids: &HashSet<AccountId>,
    ) -> Result<ProbeSelectionState, ProfileAuthRouterError> {
        router.reload_at_turn_boundary().await?;
        let registry = router.registry_snapshot()?;
        let authenticated_accounts = router
            .manager_snapshot()?
            .into_iter()
            .filter(|(_, manager)| manager.auth_cached().is_some())
            .map(|(account_id, _)| account_id)
            .filter(|account_id| !excluded_account_ids.contains(account_id))
            .collect();
        let current_account_id = {
            let selected = self
                .inner
                .selected_account
                .lock()
                .map_err(|_| router_state_error())?;
            if selected.observed_default_account_id != registry.default_account_id {
                registry.default_account_id.clone()
            } else {
                selected
                    .account_id
                    .clone()
                    .or_else(|| registry.default_account_id.clone())
            }
        };
        let cache = self
            .inner
            .limit_cache
            .lock()
            .map_err(|_| router_state_error())?
            .clone();
        Ok(ProbeSelectionState {
            registry,
            current_account_id,
            authenticated_accounts,
            cache,
            now: Utc::now().timestamp(),
        })
    }

    fn finish_probed_selection(
        &self,
        router: &ProfileAuthRouter,
        state: ProbeSelectionState,
        disposition: SelectionStateDisposition,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError> {
        let lease = router.lease_for_turn_with_authenticated_accounts(
            &state.cache,
            state.current_account_id.as_ref(),
            /*relevant_limit_id*/ None,
            state.now,
            MAX_LIMIT_AGE_SECONDS,
            &state.authenticated_accounts,
        )?;
        if matches!(disposition, SelectionStateDisposition::Commit) {
            let mut selected = self
                .inner
                .selected_account
                .lock()
                .map_err(|_| router_state_error())?;
            selected.account_id = Some(lease.account_id().clone());
            selected.observed_default_account_id = state.registry.default_account_id;
        }
        Ok(Some(lease))
    }

    fn finish_probed_selection_or_none(
        &self,
        router: &ProfileAuthRouter,
        state: ProbeSelectionState,
        disposition: SelectionStateDisposition,
        return_none_when_exhausted: bool,
    ) -> Result<Option<AccountLease>, ProfileAuthRouterError> {
        match self.finish_probed_selection(router, state, disposition) {
            Err(ProfileAuthRouterError::Selection(SelectionError::NoEligibleAccount {
                ..
            })) if return_none_when_exhausted => Ok(None),
            result => result,
        }
    }
}

fn next_probe_target(
    state: &ProbeSelectionState,
    attempted: &HashSet<AccountId>,
) -> Option<AccountId> {
    let mut current_eligible_priority = None;
    if let Some(current_id) = state.current_account_id.as_ref()
        && let Some(current) = state
            .registry
            .accounts
            .iter()
            .find(|account| &account.id == current_id)
        && current.enabled
        && state.authenticated_accounts.contains(current_id)
        && supports_automatic_selection(current.auth_mode)
    {
        match state.cache.eligibility(
            current_id,
            /*relevant_limit_id*/ None,
            state.now,
            MAX_LIMIT_AGE_SECONDS,
        ) {
            Eligibility::Eligible => current_eligible_priority = Some(current.priority),
            Eligibility::Unknown(_) if !attempted.contains(current_id) => {
                return Some(current_id.clone());
            }
            Eligibility::Reached(_) | Eligibility::Unknown(_) => {}
        }
    }

    for account in state.registry.enabled_by_priority() {
        if Some(&account.id) == state.current_account_id.as_ref()
            || !state.authenticated_accounts.contains(&account.id)
            || !supports_automatic_selection(account.auth_mode)
        {
            continue;
        }
        // Equal and lower tiers cannot displace an eligible current account, so probing them
        // would add credential traffic without changing this turn's selection.
        if current_eligible_priority.is_some_and(|priority| account.priority <= priority) {
            return None;
        }
        match state.cache.eligibility(
            &account.id,
            /*relevant_limit_id*/ None,
            state.now,
            MAX_LIMIT_AGE_SECONDS,
        ) {
            Eligibility::Eligible => return None,
            Eligibility::Unknown(_) if !attempted.contains(&account.id) => {
                return Some(account.id.clone());
            }
            Eligibility::Reached(_) | Eligibility::Unknown(_) => {}
        }
    }
    None
}
