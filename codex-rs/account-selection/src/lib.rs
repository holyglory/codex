use codex_account_registry::AccountId;
use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;
use codex_account_registry::SelectionPolicy;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::RateLimitSnapshot;
use std::collections::HashMap;
use std::collections::HashSet;
use thiserror::Error;

const DEFAULT_LIMIT_ID: &str = "codex";
const MAX_LIMIT_SNAPSHOTS: usize = 64;
const MAX_LIMIT_ID_BYTES: usize = 64;
const MAX_LIMIT_NAME_BYTES: usize = 128;
const MAX_SPEND_VALUE_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq)]
struct CachedLimits {
    observed_at: i64,
    snapshots: Vec<RateLimitSnapshot>,
    observed_at_by_limit_id: HashMap<String, i64>,
}

#[derive(Clone, Debug, Default)]
pub struct AccountLimitCache {
    entries: HashMap<AccountId, CachedLimits>,
}

impl AccountLimitCache {
    pub fn update(
        &mut self,
        account_id: AccountId,
        observed_at: i64,
        snapshots: Vec<RateLimitSnapshot>,
    ) -> Result<CacheUpdate, LimitCacheError> {
        let snapshots = normalize_snapshots(snapshots)?;
        if let Some(existing) = self.entries.get(&account_id) {
            if observed_at < existing.observed_at {
                return Err(LimitCacheError::OlderObservation);
            }
            if observed_at == existing.observed_at {
                if snapshots == existing.snapshots {
                    return Ok(CacheUpdate::Unchanged);
                }
                return Err(LimitCacheError::ConflictingObservation);
            }
        }
        self.entries.insert(
            account_id,
            CachedLimits {
                observed_at,
                observed_at_by_limit_id: snapshots
                    .iter()
                    .filter_map(|snapshot| {
                        snapshot
                            .limit_id
                            .clone()
                            .map(|limit_id| (limit_id, observed_at))
                    })
                    .collect(),
                snapshots,
            },
        );
        Ok(CacheUpdate::Updated)
    }

    /// Merges independently emitted limit buckets without refreshing buckets that were not seen.
    ///
    /// Streaming transports emit one snapshot per event, while account-limit reads use [`Self::update`]
    /// to replace a complete observation. Each merged bucket retains its own observation timestamp
    /// so a fresh event for one limit cannot make an older limit appear fresh.
    pub fn observe(
        &mut self,
        account_id: AccountId,
        observed_at: i64,
        snapshots: Vec<RateLimitSnapshot>,
    ) -> Result<CacheUpdate, LimitCacheError> {
        let snapshots = normalize_snapshots(snapshots)?;
        let existing = self.entries.get(&account_id);
        let existing_ids = existing
            .map(|entry| entry.snapshots.len())
            .unwrap_or_default();
        let new_ids = snapshots
            .iter()
            .filter(|snapshot| {
                existing.is_none_or(|entry| {
                    !entry
                        .snapshots
                        .iter()
                        .any(|cached| cached.limit_id == snapshot.limit_id)
                })
            })
            .count();
        if existing_ids.saturating_add(new_ids) > MAX_LIMIT_SNAPSHOTS {
            return Err(LimitCacheError::TooManySnapshots);
        }

        let mut changed = false;
        if let Some(existing) = existing {
            for snapshot in &snapshots {
                let Some(limit_id) = snapshot.limit_id.as_deref() else {
                    return Err(LimitCacheError::InvalidLimitId);
                };
                let Some(existing_observed_at) =
                    existing.observed_at_by_limit_id.get(limit_id).copied()
                else {
                    changed = true;
                    continue;
                };
                if observed_at < existing_observed_at {
                    return Err(LimitCacheError::OlderObservation);
                }
                let existing_snapshot = existing
                    .snapshots
                    .iter()
                    .find(|cached| cached.limit_id.as_deref() == Some(limit_id))
                    .ok_or(LimitCacheError::ConflictingObservation)?;
                if observed_at == existing_observed_at {
                    if snapshot != existing_snapshot {
                        return Err(LimitCacheError::ConflictingObservation);
                    }
                } else {
                    changed = true;
                }
            }
        } else {
            changed = !snapshots.is_empty();
        }
        if !changed {
            return Ok(CacheUpdate::Unchanged);
        }

        let entry = self
            .entries
            .entry(account_id)
            .or_insert_with(|| CachedLimits {
                observed_at,
                snapshots: Vec::new(),
                observed_at_by_limit_id: HashMap::new(),
            });
        entry.observed_at = entry.observed_at.max(observed_at);
        for snapshot in snapshots {
            let Some(limit_id) = snapshot.limit_id.clone() else {
                return Err(LimitCacheError::InvalidLimitId);
            };
            if let Some(existing) = entry
                .snapshots
                .iter_mut()
                .find(|cached| cached.limit_id.as_deref() == Some(limit_id.as_str()))
            {
                *existing = snapshot;
            } else {
                entry.snapshots.push(snapshot);
            }
            entry.observed_at_by_limit_id.insert(limit_id, observed_at);
        }
        entry
            .snapshots
            .sort_by(|left, right| left.limit_id.cmp(&right.limit_id));
        Ok(CacheUpdate::Updated)
    }

    pub fn remove(&mut self, account_id: &AccountId) {
        self.entries.remove(account_id);
    }

    pub fn eligibility(
        &self,
        account_id: &AccountId,
        relevant_limit_id: Option<&str>,
        now: i64,
        max_age_seconds: i64,
    ) -> Eligibility {
        let Some(cached) = self.entries.get(account_id) else {
            return Eligibility::Unknown(UnknownReason::Unobserved);
        };
        let observed_at = snapshot_observed_at(cached, relevant_limit_id);
        let Some(observed_at) = observed_at else {
            return Eligibility::Unknown(UnknownReason::RelevantLimitUnavailable);
        };
        if observed_at > now {
            return Eligibility::Unknown(UnknownReason::ClockSkew);
        }
        let Some(age) = now.checked_sub(observed_at) else {
            return Eligibility::Unknown(UnknownReason::ClockSkew);
        };
        if max_age_seconds < 0 || age > max_age_seconds {
            return Eligibility::Unknown(UnknownReason::Stale);
        }
        let snapshot = match relevant_limit_id {
            Some(limit_id) => cached
                .snapshots
                .iter()
                .find(|snapshot| snapshot.limit_id.as_deref() == Some(limit_id)),
            None => cached
                .snapshots
                .iter()
                .find(|snapshot| snapshot.limit_id.as_deref() == Some(DEFAULT_LIMIT_ID)),
        };
        let Some(snapshot) = snapshot else {
            return Eligibility::Unknown(UnknownReason::RelevantLimitUnavailable);
        };
        evaluate_snapshot(snapshot, now)
    }
}

fn normalize_snapshots(
    mut snapshots: Vec<RateLimitSnapshot>,
) -> Result<Vec<RateLimitSnapshot>, LimitCacheError> {
    if snapshots.len() > MAX_LIMIT_SNAPSHOTS {
        return Err(LimitCacheError::TooManySnapshots);
    }
    for snapshot in &mut snapshots {
        if snapshot.limit_id.is_none() {
            snapshot.limit_id = Some(DEFAULT_LIMIT_ID.to_string());
        }
        validate_snapshot(snapshot)?;
    }
    snapshots.sort_by(|left, right| left.limit_id.cmp(&right.limit_id));
    let mut limit_ids = HashSet::new();
    for snapshot in &snapshots {
        let Some(limit_id) = snapshot.limit_id.as_ref() else {
            return Err(LimitCacheError::InvalidLimitId);
        };
        if !limit_ids.insert(limit_id.clone()) {
            return Err(LimitCacheError::DuplicateLimitId);
        }
    }
    Ok(snapshots)
}

fn snapshot_observed_at(cached: &CachedLimits, relevant_limit_id: Option<&str>) -> Option<i64> {
    let limit_id = relevant_limit_id.unwrap_or(DEFAULT_LIMIT_ID);
    cached.observed_at_by_limit_id.get(limit_id).copied()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheUpdate {
    Updated,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum LimitCacheError {
    #[error("rate-limit observation contains too many snapshots")]
    TooManySnapshots,
    #[error("rate-limit observation contains an invalid limit identifier")]
    InvalidLimitId,
    #[error("rate-limit observation contains an invalid limit name")]
    InvalidLimitName,
    #[error("rate-limit observation contains an invalid spend value")]
    InvalidSpendValue,
    #[error("rate-limit observation contains an invalid percentage")]
    InvalidPercentage,
    #[error("rate-limit observation is older than the cached state")]
    OlderObservation,
    #[error("rate-limit observation conflicts at the same timestamp")]
    ConflictingObservation,
    #[error("rate-limit observation contains duplicate limit identifiers")]
    DuplicateLimitId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReachedReason {
    RateLimit,
    SpendControl,
    Credits,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownReason {
    Unobserved,
    Stale,
    ClockSkew,
    RelevantLimitUnavailable,
    InsufficientEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Eligibility {
    Eligible,
    Reached(ReachedReason),
    Unknown(UnknownReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionReason {
    Pinned,
    CurrentEligible,
    ManualCurrent,
    ManualDefault,
    CurrentReached(ReachedReason),
    CurrentUnknown(UnknownReason),
    CurrentDisabled,
    CurrentUnauthenticated,
    CurrentUnsupportedAuth,
    NoCurrentAccount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionDecision {
    pub account_id: AccountId,
    pub switched: bool,
    pub reason: SelectionReason,
}

#[derive(Clone, Debug)]
pub struct SelectionRequest<'a> {
    pub current_account_id: Option<&'a AccountId>,
    pub pinned_account_id: Option<&'a AccountId>,
    pub authenticated_accounts: &'a HashSet<AccountId>,
    pub relevant_limit_id: Option<&'a str>,
    pub now: i64,
    pub max_limit_age_seconds: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum SelectionError {
    #[error("selected account is unknown")]
    UnknownAccount,
    #[error("selected account is disabled")]
    DisabledAccount,
    #[error("selected account is not authenticated")]
    NotAuthenticated,
    #[error("selected account has reached its limit")]
    LimitReached(ReachedReason),
    #[error("selected account capacity is unknown")]
    CapacityUnknown(UnknownReason),
    #[error("no eligible account is available")]
    NoEligibleAccount { current: SelectionReason },
}

pub fn select_account(
    registry: &AccountRegistry,
    cache: &AccountLimitCache,
    request: SelectionRequest<'_>,
) -> Result<SelectionDecision, SelectionError> {
    if let Some(pinned_id) = request.pinned_account_id {
        let account = account_by_id(registry, pinned_id)?;
        require_enabled_and_authenticated(account, request.authenticated_accounts)?;
        match cache.eligibility(
            pinned_id,
            request.relevant_limit_id,
            request.now,
            request.max_limit_age_seconds,
        ) {
            Eligibility::Eligible => {}
            Eligibility::Reached(reason) => return Err(SelectionError::LimitReached(reason)),
            Eligibility::Unknown(reason) => return Err(SelectionError::CapacityUnknown(reason)),
        }
        return Ok(SelectionDecision {
            account_id: pinned_id.clone(),
            switched: request.current_account_id != Some(pinned_id),
            reason: SelectionReason::Pinned,
        });
    }

    let current = request
        .current_account_id
        .and_then(|id| registry.accounts.iter().find(|account| &account.id == id));
    if !registry.auto_selection.enabled {
        let (account, reason) = match current {
            Some(account) => (account, SelectionReason::ManualCurrent),
            None => (
                registry
                    .default_account_id
                    .as_ref()
                    .and_then(|id| registry.accounts.iter().find(|account| &account.id == id))
                    .ok_or(SelectionError::UnknownAccount)?,
                SelectionReason::ManualDefault,
            ),
        };
        require_enabled_and_authenticated(account, request.authenticated_accounts)?;
        if let Eligibility::Reached(reason) = cache.eligibility(
            &account.id,
            request.relevant_limit_id,
            request.now,
            request.max_limit_age_seconds,
        ) {
            return Err(SelectionError::LimitReached(reason));
        }
        return Ok(SelectionDecision {
            account_id: account.id.clone(),
            switched: request.current_account_id != Some(&account.id),
            reason,
        });
    }

    let current_reason = current_reason(current, cache, &request);
    match registry.auto_selection.policy {
        SelectionPolicy::Priority => {
            for account in registry.enabled_by_priority() {
                if !request.authenticated_accounts.contains(&account.id)
                    || !supports_automatic_selection(account.auth_mode)
                {
                    continue;
                }
                if cache.eligibility(
                    &account.id,
                    request.relevant_limit_id,
                    request.now,
                    request.max_limit_age_seconds,
                ) == Eligibility::Eligible
                {
                    // Keep a live account within the winning tier so equal priorities do not
                    // cause an account switch on every turn.
                    if current_reason == SelectionReason::CurrentEligible
                        && let Some(current) = current
                        && current.priority == account.priority
                    {
                        return Ok(SelectionDecision {
                            account_id: current.id.clone(),
                            switched: false,
                            reason: current_reason,
                        });
                    }
                    return Ok(SelectionDecision {
                        account_id: account.id.clone(),
                        switched: request.current_account_id != Some(&account.id),
                        reason: current_reason,
                    });
                }
            }
        }
    }
    Err(SelectionError::NoEligibleAccount {
        current: current_reason,
    })
}

fn validate_snapshot(snapshot: &RateLimitSnapshot) -> Result<(), LimitCacheError> {
    let Some(limit_id) = snapshot.limit_id.as_deref() else {
        return Err(LimitCacheError::InvalidLimitId);
    };
    if !valid_bounded_string(limit_id, MAX_LIMIT_ID_BYTES) {
        return Err(LimitCacheError::InvalidLimitId);
    }
    if snapshot
        .limit_name
        .as_deref()
        .is_some_and(|name| !valid_bounded_string(name, MAX_LIMIT_NAME_BYTES))
    {
        return Err(LimitCacheError::InvalidLimitName);
    }
    for window in [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
    {
        if !window.used_percent.is_finite() || !(0.0..=100.0).contains(&window.used_percent) {
            return Err(LimitCacheError::InvalidPercentage);
        }
    }
    if snapshot
        .credits
        .as_ref()
        .and_then(|credits| credits.balance.as_deref())
        .is_some_and(|balance| !valid_bounded_string(balance, MAX_SPEND_VALUE_BYTES))
    {
        return Err(LimitCacheError::InvalidSpendValue);
    }
    if let Some(individual) = &snapshot.individual_limit {
        if !(0..=100).contains(&individual.remaining_percent) {
            return Err(LimitCacheError::InvalidPercentage);
        }
        if !valid_bounded_string(&individual.limit, MAX_SPEND_VALUE_BYTES)
            || !valid_bounded_string(&individual.used, MAX_SPEND_VALUE_BYTES)
        {
            return Err(LimitCacheError::InvalidSpendValue);
        }
    }
    Ok(())
}

fn valid_bounded_string(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn account_by_id<'a>(
    registry: &'a AccountRegistry,
    account_id: &AccountId,
) -> Result<&'a AccountMetadata, SelectionError> {
    registry
        .accounts
        .iter()
        .find(|account| &account.id == account_id)
        .ok_or(SelectionError::UnknownAccount)
}

fn require_enabled_and_authenticated(
    account: &AccountMetadata,
    authenticated: &HashSet<AccountId>,
) -> Result<(), SelectionError> {
    if !account.enabled {
        return Err(SelectionError::DisabledAccount);
    }
    if !authenticated.contains(&account.id) {
        return Err(SelectionError::NotAuthenticated);
    }
    Ok(())
}

fn current_reason(
    current: Option<&AccountMetadata>,
    cache: &AccountLimitCache,
    request: &SelectionRequest<'_>,
) -> SelectionReason {
    let Some(current) = current else {
        return SelectionReason::NoCurrentAccount;
    };
    if !current.enabled {
        return SelectionReason::CurrentDisabled;
    }
    if !request.authenticated_accounts.contains(&current.id) {
        return SelectionReason::CurrentUnauthenticated;
    }
    if !supports_automatic_selection(current.auth_mode) {
        return SelectionReason::CurrentUnsupportedAuth;
    }
    match cache.eligibility(
        &current.id,
        request.relevant_limit_id,
        request.now,
        request.max_limit_age_seconds,
    ) {
        Eligibility::Eligible => SelectionReason::CurrentEligible,
        Eligibility::Reached(reason) => SelectionReason::CurrentReached(reason),
        Eligibility::Unknown(reason) => SelectionReason::CurrentUnknown(reason),
    }
}

/// Returns whether a locally managed profile may be entered by automatic selection.
///
/// This is intentionally narrower than backend-capability classifications: only managed ChatGPT
/// OAuth profiles are eligible for automatic selection or availability probing.
pub fn supports_automatic_selection(auth_mode: AuthMode) -> bool {
    match auth_mode {
        AuthMode::Chatgpt => true,
        AuthMode::ApiKey
        | AuthMode::ChatgptAuthTokens
        | AuthMode::Headers
        | AuthMode::AgentIdentity
        | AuthMode::PersonalAccessToken
        | AuthMode::BedrockApiKey
        | AuthMode::BedrockAccessKeys => false,
    }
}

fn evaluate_snapshot(snapshot: &RateLimitSnapshot, now: i64) -> Eligibility {
    if let Some(reached_type) = snapshot.rate_limit_reached_type {
        return Eligibility::Reached(match reached_type {
            codex_protocol::protocol::RateLimitReachedType::RateLimitReached => {
                ReachedReason::RateLimit
            }
            codex_protocol::protocol::RateLimitReachedType::WorkspaceOwnerCreditsDepleted
            | codex_protocol::protocol::RateLimitReachedType::WorkspaceMemberCreditsDepleted => {
                ReachedReason::Credits
            }
            codex_protocol::protocol::RateLimitReachedType::WorkspaceOwnerUsageLimitReached
            | codex_protocol::protocol::RateLimitReachedType::WorkspaceMemberUsageLimitReached => {
                ReachedReason::SpendControl
            }
        });
    }
    if snapshot.spend_control_reached == Some(true) {
        return Eligibility::Reached(ReachedReason::SpendControl);
    }
    let depleted_credits = snapshot
        .credits
        .as_ref()
        .is_some_and(|credits| !credits.unlimited && !credits.has_credits);
    let mut eligibility_evidence = snapshot
        .credits
        .as_ref()
        .is_some_and(|credits| credits.unlimited || credits.has_credits);
    let mut unknown_evidence = false;
    for window in [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
    {
        if !window.used_percent.is_finite()
            || window.used_percent < 0.0
            || window.window_minutes.is_some_and(|minutes| minutes <= 0)
        {
            unknown_evidence = true;
            continue;
        }
        if window.resets_at.is_some_and(|resets_at| resets_at <= now) {
            unknown_evidence = true;
            continue;
        }
        if window.used_percent >= 100.0 {
            return Eligibility::Reached(ReachedReason::RateLimit);
        }
        eligibility_evidence = true;
    }
    if let Some(individual) = &snapshot.individual_limit {
        if !(0..=100).contains(&individual.remaining_percent) || individual.resets_at <= now {
            unknown_evidence = true;
        } else if individual.remaining_percent == 0 {
            return Eligibility::Reached(ReachedReason::SpendControl);
        } else {
            eligibility_evidence = true;
        }
    }
    if unknown_evidence {
        Eligibility::Unknown(UnknownReason::InsufficientEvidence)
    } else if eligibility_evidence {
        Eligibility::Eligible
    } else if depleted_credits {
        Eligibility::Reached(ReachedReason::Credits)
    } else {
        Eligibility::Unknown(UnknownReason::InsufficientEvidence)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
