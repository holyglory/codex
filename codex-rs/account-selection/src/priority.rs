use std::cmp::Reverse;

use codex_account_registry::AccountMetadata;
use codex_account_registry::AccountRegistry;

use super::AccountLimitCache;
use super::DEFAULT_LIMIT_ID;
use super::Eligibility;
use super::SelectionRequest;
use super::supports_automatic_selection;

const MIN_RESET_ADVANTAGE_SECONDS: i64 = 10 * 60;

pub(super) fn select_by_priority<'a>(
    registry: &'a AccountRegistry,
    cache: &AccountLimitCache,
    request: &SelectionRequest<'_>,
) -> Option<&'a AccountMetadata> {
    let limit_id = request.relevant_limit_id.unwrap_or(DEFAULT_LIMIT_ID);
    let mut current = None;
    let (selected, reset) = registry
        .accounts
        .iter()
        .filter(|account| {
            account.enabled
                && request.authenticated_accounts.contains(&account.id)
                && supports_automatic_selection(account.auth_mode)
                && cache.eligibility(
                    &account.id,
                    request.relevant_limit_id,
                    request.now,
                    request.max_limit_age_seconds,
                ) == Eligibility::Eligible
        })
        .map(|account| {
            let reset = cache
                .entries
                .get(&account.id)
                .and_then(|cached| {
                    cached
                        .snapshots
                        .iter()
                        .find(|snapshot| snapshot.limit_id.as_deref() == Some(limit_id))
                })
                .and_then(|snapshot| {
                    // For eligible accounts, used windows determine the next reset before
                    // unused windows. Missing reset evidence must not select a healthier window.
                    let windows = [
                        snapshot
                            .primary
                            .as_ref()
                            .map(|window| (window.used_percent > 0.0, window.resets_at)),
                        snapshot
                            .secondary
                            .as_ref()
                            .map(|window| (window.used_percent > 0.0, window.resets_at)),
                        snapshot
                            .individual_limit
                            .as_ref()
                            .map(|limit| (limit.remaining_percent < 100, Some(limit.resets_at))),
                    ];
                    let used = windows.iter().flatten().any(|(used, _)| *used);
                    windows
                        .into_iter()
                        .flatten()
                        .filter(|(is_used, _)| *is_used == used)
                        .filter_map(|(_, reset)| reset)
                        .filter(|reset| *reset > request.now)
                        .min()
                });
            if request.current_account_id == Some(&account.id) {
                current = Some((account, reset));
            }
            (account, reset)
        })
        .max_by_key(|(account, reset)| {
            (
                account.priority,
                *reset,
                request.current_account_id == Some(&account.id),
                Reverse(&account.id),
            )
        })?;
    if let Some((current, Some(current_reset))) = current
        && current.priority == selected.priority
        && let Some(reset) = reset
        && reset.saturating_sub(current_reset) < MIN_RESET_ADVANTAGE_SECONDS
    {
        return Some(current);
    }
    Some(selected)
}
