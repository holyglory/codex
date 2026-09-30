//! Automatic routing distinguishes included allowance from explicitly permitted credits.
use codex_account_registry::AccountMetadata;
use codex_protocol::protocol::RateLimitSnapshot;

use crate::AccountLimitCache;
use crate::Eligibility;
use crate::ReachedReason;
use crate::UnknownReason;
use crate::evaluate_snapshot;

/// Fresh capacity available to automatic routing, with its funding source kept distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticCapacity {
    Included,
    Credits,
    Reached(ReachedReason),
    Unknown(UnknownReason),
}

impl AccountLimitCache {
    /// Applies the profile's credit permission without changing manual or pinned selection.
    pub fn automatic_capacity(
        &self,
        account: &AccountMetadata,
        relevant_limit_id: Option<&str>,
        now: i64,
        max_age_seconds: i64,
    ) -> AutomaticCapacity {
        match self.fresh_snapshot(&account.id, relevant_limit_id, now, max_age_seconds) {
            Ok(snapshot) => classify(snapshot, account, now),
            Err(reason) => AutomaticCapacity::Unknown(reason),
        }
    }
}

fn classify(
    snapshot: &RateLimitSnapshot,
    account: &AccountMetadata,
    now: i64,
) -> AutomaticCapacity {
    // Explicit service denials and spending restrictions always beat a credit balance.
    if (snapshot.rate_limit_reached_type.is_some() || snapshot.spend_control_reached == Some(true))
        && let Eligibility::Reached(reason) = evaluate_snapshot(snapshot, now)
    {
        return AutomaticCapacity::Reached(reason);
    }
    if let Some(limit) = &snapshot.individual_limit {
        if !(0..=100).contains(&limit.remaining_percent) || limit.resets_at <= now {
            return AutomaticCapacity::Unknown(UnknownReason::InsufficientEvidence);
        }
        if limit.remaining_percent == 0 {
            return AutomaticCapacity::Reached(ReachedReason::SpendControl);
        }
    }
    let mut has_window = false;
    let mut exhausted = false;
    for window in snapshot.primary.iter().chain(snapshot.secondary.iter()) {
        if !window.used_percent.is_finite()
            || !(0.0..=100.0).contains(&window.used_percent)
            || window.window_minutes.is_some_and(|minutes| minutes <= 0)
            || window.resets_at.is_some_and(|reset| reset <= now)
        {
            return AutomaticCapacity::Unknown(UnknownReason::InsufficientEvidence);
        }
        has_window = true;
        exhausted |= window.used_percent >= 100.0;
    }
    if has_window && !exhausted {
        return AutomaticCapacity::Included;
    }
    if let Some(credits) = &snapshot.credits {
        if account.credit_usage_enabled && (credits.unlimited || credits.has_credits) {
            return AutomaticCapacity::Credits;
        }
        return AutomaticCapacity::Reached(if exhausted {
            ReachedReason::RateLimit
        } else {
            ReachedReason::Credits
        });
    }
    if exhausted {
        AutomaticCapacity::Reached(ReachedReason::RateLimit)
    } else {
        AutomaticCapacity::Unknown(UnknownReason::InsufficientEvidence)
    }
}
