use super::*;
use chrono::TimeZone;
use chrono::Utc;
use codex_account_registry::AccountAlias;
use codex_account_registry::AccountMetadata;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::CreditsSnapshot;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::SpendControlLimitSnapshot;
use pretty_assertions::assert_eq;
use std::str::FromStr;

fn account(alias: &str, priority: u32) -> AccountMetadata {
    account_with_mode(alias, priority, AuthMode::Chatgpt)
}

fn account_with_mode(alias: &str, priority: u32, auth_mode: AuthMode) -> AccountMetadata {
    let mut account = AccountMetadata::new(
        AccountAlias::from_str(alias).expect("alias"),
        auth_mode,
        Utc.timestamp_opt(1, 0).single().expect("timestamp"),
    );
    account.priority = priority;
    account
}

fn assert_invalid_snapshot(snapshot: RateLimitSnapshot, expected: LimitCacheError) {
    let mut cache = AccountLimitCache::default();
    assert_eq!(
        cache.update(
            AccountId::generate(),
            /*observed_at*/ 1_000,
            vec![snapshot]
        ),
        Err(expected)
    );
}

fn snapshot(limit_id: &str, used_percent: f64) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some(limit_id.to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent,
            window_minutes: Some(300),
            resets_at: Some(2_000),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: Some(false),
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

fn request<'a>(
    current: Option<&'a AccountId>,
    pinned: Option<&'a AccountId>,
    authenticated: &'a HashSet<AccountId>,
) -> SelectionRequest<'a> {
    SelectionRequest {
        current_account_id: current,
        pinned_account_id: pinned,
        authenticated_accounts: authenticated,
        relevant_limit_id: Some("codex"),
        now: 1_100,
        max_limit_age_seconds: 300,
    }
}

#[test]
fn keeps_an_eligible_current_account() {
    let current = account("current", /*priority*/ 10);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add");
    registry.default_account_id = Some(current.id.clone());
    let authenticated = HashSet::from([current.id.clone()]);
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            current.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 25.0)],
        )
        .expect("update cache");

    assert_eq!(
        SelectionDecision {
            account_id: current.id.clone(),
            switched: false,
            reason: SelectionReason::CurrentEligible,
        },
        select_account(
            &registry,
            &cache,
            request(Some(&current.id), /*pinned*/ None, &authenticated)
        )
        .expect("selection")
    );
}

#[test]
fn reached_current_switches_to_highest_numeric_eligible_priority() {
    let current = account("current", /*priority*/ 1_000);
    let lower = account("lower", /*priority*/ 100);
    let higher = account("higher", /*priority*/ 900);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    for account in [&current, &lower, &higher] {
        registry.add_account(account.clone()).expect("add");
    }
    let authenticated = HashSet::from([current.id.clone(), lower.id.clone(), higher.id.clone()]);
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            current.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 100.0)],
        )
        .expect("update current");
    cache
        .update(
            lower.id,
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 20.0)],
        )
        .expect("update lower");
    cache
        .update(
            higher.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 10.0)],
        )
        .expect("update higher");

    let decision = select_account(
        &registry,
        &cache,
        request(Some(&current.id), /*pinned*/ None, &authenticated),
    )
    .expect("selection");
    assert_eq!(higher.id, decision.account_id);
    assert_eq!(
        SelectionReason::CurrentReached(ReachedReason::RateLimit),
        decision.reason
    );
}

#[test]
fn automatic_selection_drains_priorities_in_descending_order() {
    let highest = account("highest", /*priority*/ 1_000);
    let middle = account("middle", /*priority*/ 500);
    let lowest = account("lowest", /*priority*/ 1);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    for account in [&lowest, &highest, &middle] {
        registry.add_account(account.clone()).expect("add account");
    }
    let authenticated = HashSet::from([highest.id.clone(), middle.id.clone(), lowest.id.clone()]);
    let mut cache = AccountLimitCache::default();
    for account in [&highest, &middle, &lowest] {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 10.0)],
            )
            .expect("cache eligible account");
    }

    let mut selected = Vec::new();
    for account in [&highest, &middle, &lowest] {
        selected.push(
            select_account(
                &registry,
                &cache,
                request(/*current*/ None, /*pinned*/ None, &authenticated),
            )
            .expect("select next priority")
            .account_id,
        );
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_001,
                vec![snapshot("codex", /*used_percent*/ 100.0)],
            )
            .expect("exhaust selected account");
    }

    assert_eq!(selected, vec![highest.id, middle.id, lowest.id]);
}

#[test]
fn eligible_lower_priority_current_switches_to_higher_priority() {
    let current = account("current", /*priority*/ 1);
    let higher = account("higher", /*priority*/ 1_000);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add current");
    registry.add_account(higher.clone()).expect("add higher");
    let authenticated = HashSet::from([current.id.clone(), higher.id.clone()]);
    let mut cache = AccountLimitCache::default();
    for account in [&current, &higher] {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 10.0)],
            )
            .expect("cache eligible account");
    }

    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(Some(&current.id), /*pinned*/ None, &authenticated),
        ),
        Ok(SelectionDecision {
            account_id: higher.id,
            switched: true,
            reason: SelectionReason::CurrentEligible,
        })
    );
}

#[test]
fn unknown_or_stale_candidates_are_not_assumed_eligible() {
    let current = account("current", /*priority*/ 1);
    let candidate = account("candidate", /*priority*/ 2);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add");
    registry.add_account(candidate.clone()).expect("add");
    let authenticated = HashSet::from([current.id.clone(), candidate.id.clone()]);
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            current.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 100.0)],
        )
        .expect("update current");
    cache
        .update(
            candidate.id,
            /*observed_at*/ 100,
            vec![snapshot("codex", /*used_percent*/ 1.0)],
        )
        .expect("update candidate");

    assert_eq!(
        Err(SelectionError::NoEligibleAccount {
            current: SelectionReason::CurrentReached(ReachedReason::RateLimit),
        }),
        select_account(
            &registry,
            &cache,
            request(Some(&current.id), /*pinned*/ None, &authenticated)
        )
    );
}

#[test]
fn a_pin_never_falls_back_or_assumes_another_account() {
    let pinned = account("pinned", /*priority*/ 1);
    let fallback = account("fallback", /*priority*/ 2);
    let mut registry = AccountRegistry::default();
    registry.add_account(pinned.clone()).expect("add");
    registry.add_account(fallback.clone()).expect("add");
    let authenticated = HashSet::from([pinned.id.clone(), fallback.id.clone()]);
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            pinned.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 100.0)],
        )
        .expect("update pinned");
    cache
        .update(
            fallback.id,
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 1.0)],
        )
        .expect("update fallback");

    assert_eq!(
        Err(SelectionError::LimitReached(ReachedReason::RateLimit)),
        select_account(
            &registry,
            &cache,
            request(Some(&pinned.id), Some(&pinned.id), &authenticated)
        )
    );
}

#[test]
fn open_subscription_window_remains_eligible_without_top_up_credits() {
    let mut limit = snapshot("codex", /*used_percent*/ 1.0);
    limit.credits = Some(CreditsSnapshot {
        has_credits: false,
        unlimited: false,
        balance: None,
    });
    assert_eq!(
        Eligibility::Eligible,
        evaluate_snapshot(&limit, /*now*/ 1_100)
    );

    limit.primary = None;
    assert_eq!(
        Eligibility::Reached(ReachedReason::Credits),
        evaluate_snapshot(&limit, /*now*/ 1_100)
    );
}

#[test]
fn spend_control_is_reached_without_credit_evidence() {
    let mut limit = snapshot("codex", /*used_percent*/ 1.0);
    limit.credits = None;
    limit.spend_control_reached = Some(true);
    assert_eq!(
        Eligibility::Reached(ReachedReason::SpendControl),
        evaluate_snapshot(&limit, /*now*/ 1_100)
    );
}

#[test]
fn cache_rejects_older_conflicting_and_duplicate_observations() {
    let account = account("account", /*priority*/ 1);
    let mut cache = AccountLimitCache::default();
    let current = vec![snapshot("codex", /*used_percent*/ 25.0)];
    assert_eq!(
        Ok(CacheUpdate::Updated),
        cache.update(
            account.id.clone(),
            /*observed_at*/ 1_000,
            current.clone()
        )
    );
    assert_eq!(
        Ok(CacheUpdate::Unchanged),
        cache.update(account.id.clone(), /*observed_at*/ 1_000, current)
    );
    assert_eq!(
        Err(LimitCacheError::ConflictingObservation),
        cache.update(
            account.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 50.0)]
        )
    );
    assert_eq!(
        Err(LimitCacheError::OlderObservation),
        cache.update(
            account.id.clone(),
            /*observed_at*/ 999,
            vec![snapshot("codex", /*used_percent*/ 10.0)]
        )
    );
    assert_eq!(
        Err(LimitCacheError::DuplicateLimitId),
        cache.update(
            account.id,
            /*observed_at*/ 1_001,
            vec![
                snapshot("codex", /*used_percent*/ 10.0),
                snapshot("codex", /*used_percent*/ 20.0)
            ]
        )
    );
}

#[test]
fn streamed_buckets_merge_without_refreshing_unobserved_buckets() {
    let account = account("account", /*priority*/ 1);
    let mut cache = AccountLimitCache::default();
    cache
        .observe(
            account.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 25.0)],
        )
        .expect("observe default bucket");
    cache
        .observe(
            account.id.clone(),
            /*observed_at*/ 1_100,
            vec![snapshot("future", /*used_percent*/ 10.0)],
        )
        .expect("observe future bucket");

    assert_eq!(
        cache.eligibility(
            &account.id,
            /*relevant_limit_id*/ None,
            /*now*/ 1_350,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Unknown(UnknownReason::Stale)
    );
    assert_eq!(
        cache.eligibility(
            &account.id,
            Some("future"),
            /*now*/ 1_350,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Eligible
    );
    assert_eq!(
        cache.observe(
            account.id.clone(),
            /*observed_at*/ 1_100,
            vec![snapshot("future", /*used_percent*/ 10.0)]
        ),
        Ok(CacheUpdate::Unchanged)
    );
    assert_eq!(
        cache.observe(
            account.id,
            /*observed_at*/ 1_100,
            vec![snapshot("future", /*used_percent*/ 20.0)]
        ),
        Err(LimitCacheError::ConflictingObservation)
    );
}

#[test]
fn explicit_reached_type_wins_over_invalid_windows_and_is_classified() {
    let cases = [
        (
            RateLimitReachedType::RateLimitReached,
            ReachedReason::RateLimit,
        ),
        (
            RateLimitReachedType::WorkspaceOwnerCreditsDepleted,
            ReachedReason::Credits,
        ),
        (
            RateLimitReachedType::WorkspaceMemberUsageLimitReached,
            ReachedReason::SpendControl,
        ),
    ];
    for (reached_type, expected) in cases {
        let mut limit = snapshot("codex", f64::NAN);
        limit.rate_limit_reached_type = Some(reached_type);
        assert_eq!(
            Eligibility::Reached(expected),
            evaluate_snapshot(&limit, /*now*/ 1_100)
        );
    }
}

#[test]
fn expired_and_inconsistent_windows_are_unknown() {
    let mut expired = snapshot("codex", /*used_percent*/ 25.0);
    expired.primary.as_mut().expect("window").resets_at = Some(1_000);
    assert_eq!(
        Eligibility::Unknown(UnknownReason::InsufficientEvidence),
        evaluate_snapshot(&expired, /*now*/ 1_100)
    );
    let mut invalid = snapshot("codex", f64::INFINITY);
    invalid.spend_control_reached = Some(false);
    assert_eq!(
        Eligibility::Unknown(UnknownReason::InsufficientEvidence),
        evaluate_snapshot(&invalid, /*now*/ 1_100)
    );
}

#[test]
fn individual_spend_limit_is_enforced() {
    let mut limit = snapshot("codex", /*used_percent*/ 10.0);
    limit.individual_limit = Some(SpendControlLimitSnapshot {
        limit: "100".to_string(),
        used: "100".to_string(),
        remaining_percent: 0,
        resets_at: 2_000,
    });
    assert_eq!(
        Eligibility::Reached(ReachedReason::SpendControl),
        evaluate_snapshot(&limit, /*now*/ 1_100)
    );
}

#[test]
fn manual_selection_reports_current_and_default_truthfully() {
    let current = account("current", /*priority*/ 2);
    let default = account("default", /*priority*/ 1);
    let mut registry = AccountRegistry::default();
    registry.add_account(current.clone()).expect("add current");
    registry.add_account(default.clone()).expect("add default");
    registry.default_account_id = Some(default.id.clone());
    let authenticated = HashSet::from([current.id.clone(), default.id]);
    let cache = AccountLimitCache::default();

    let retained = select_account(
        &registry,
        &cache,
        request(Some(&current.id), /*pinned*/ None, &authenticated),
    )
    .expect("retain current");
    assert_eq!(SelectionReason::ManualCurrent, retained.reason);
    assert!(!retained.switched);

    let deleted = AccountId::generate();
    let selected_default = select_account(
        &registry,
        &cache,
        request(Some(&deleted), /*pinned*/ None, &authenticated),
    )
    .expect("select default");
    assert_eq!(SelectionReason::ManualDefault, selected_default.reason);
    assert!(selected_default.switched);
}

#[test]
fn a_pin_with_unknown_capacity_fails_without_fallback() {
    let pinned = account("pinned", /*priority*/ 1);
    let mut registry = AccountRegistry::default();
    registry.add_account(pinned.clone()).expect("add");
    let authenticated = HashSet::from([pinned.id.clone()]);

    assert_eq!(
        Err(SelectionError::CapacityUnknown(UnknownReason::Unobserved)),
        select_account(
            &registry,
            &AccountLimitCache::default(),
            request(Some(&pinned.id), Some(&pinned.id), &authenticated)
        )
    );
}

#[test]
fn cache_normalizes_default_bucket_orders_snapshots_and_uses_exact_lookup() {
    let cached_account = account("account", /*priority*/ 1);
    let mut default = snapshot("placeholder", /*used_percent*/ 20.0);
    default.limit_id = None;
    let future = snapshot("future", /*used_percent*/ 30.0);
    let mut cache = AccountLimitCache::default();
    assert_eq!(
        cache.update(
            cached_account.id.clone(),
            /*observed_at*/ 1_000,
            vec![future.clone(), default.clone()]
        ),
        Ok(CacheUpdate::Updated)
    );
    assert_eq!(
        cache.entries[&cached_account.id]
            .snapshots
            .iter()
            .map(|snapshot| snapshot.limit_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("codex"), Some("future")]
    );
    default.limit_id = Some("codex".to_string());
    assert_eq!(
        cache.update(
            cached_account.id.clone(),
            /*observed_at*/ 1_000,
            vec![default, future]
        ),
        Ok(CacheUpdate::Unchanged)
    );
    assert_eq!(
        cache.eligibility(
            &cached_account.id,
            /*relevant_limit_id*/ None,
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Eligible
    );
    assert_eq!(
        cache.eligibility(
            &cached_account.id,
            Some("future"),
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Eligible
    );

    let future_only = account("future-only", /*priority*/ 2);
    cache
        .update(
            future_only.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("future", /*used_percent*/ 1.0)],
        )
        .expect("cache future bucket");
    assert_eq!(
        cache.eligibility(
            &future_only.id,
            /*relevant_limit_id*/ None,
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Unknown(UnknownReason::RelevantLimitUnavailable)
    );
    assert_eq!(
        cache.eligibility(
            &future_only.id,
            Some("codex"),
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Unknown(UnknownReason::RelevantLimitUnavailable)
    );
}

#[test]
fn cache_rejects_unbounded_strings_counts_controls_and_invalid_percentages() {
    let too_many = (0..=MAX_LIMIT_SNAPSHOTS)
        .map(|index| snapshot(&format!("limit-{index}"), /*used_percent*/ 1.0))
        .collect();
    assert_eq!(
        AccountLimitCache::default().update(
            AccountId::generate(),
            /*observed_at*/ 1_000,
            too_many
        ),
        Err(LimitCacheError::TooManySnapshots)
    );

    assert_invalid_snapshot(
        snapshot(
            &"x".repeat(MAX_LIMIT_ID_BYTES + 1),
            /*used_percent*/ 1.0,
        ),
        LimitCacheError::InvalidLimitId,
    );
    let mut invalid_name = snapshot("codex", /*used_percent*/ 1.0);
    invalid_name.limit_name = Some("unsafe\nname".to_string());
    assert_invalid_snapshot(invalid_name, LimitCacheError::InvalidLimitName);
    let mut invalid_balance = snapshot("codex", /*used_percent*/ 1.0);
    invalid_balance.credits = Some(CreditsSnapshot {
        has_credits: true,
        unlimited: false,
        balance: Some("x".repeat(MAX_SPEND_VALUE_BYTES + 1)),
    });
    assert_invalid_snapshot(invalid_balance, LimitCacheError::InvalidSpendValue);
    let mut invalid_spend = snapshot("codex", /*used_percent*/ 1.0);
    invalid_spend.individual_limit = Some(SpendControlLimitSnapshot {
        limit: "100".to_string(),
        used: "unsafe\rvalue".to_string(),
        remaining_percent: 50,
        resets_at: 2_000,
    });
    assert_invalid_snapshot(invalid_spend, LimitCacheError::InvalidSpendValue);
    for percent in [f64::NAN, f64::INFINITY, -1.0, 100.1] {
        assert_invalid_snapshot(
            snapshot("codex", percent),
            LimitCacheError::InvalidPercentage,
        );
        let mut secondary = snapshot("codex", /*used_percent*/ 1.0);
        secondary.secondary = Some(RateLimitWindow {
            used_percent: percent,
            window_minutes: Some(300),
            resets_at: Some(2_000),
        });
        assert_invalid_snapshot(secondary, LimitCacheError::InvalidPercentage);
    }
    let mut invalid_remaining = snapshot("codex", /*used_percent*/ 1.0);
    invalid_remaining.individual_limit = Some(SpendControlLimitSnapshot {
        limit: "100".to_string(),
        used: "10".to_string(),
        remaining_percent: 101,
        resets_at: 2_000,
    });
    assert_invalid_snapshot(invalid_remaining, LimitCacheError::InvalidPercentage);

    let account = account("stable", /*priority*/ 1);
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            account.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 25.0)],
        )
        .expect("cache valid state");
    assert_eq!(
        cache.update(
            account.id.clone(),
            /*observed_at*/ 1_001,
            vec![snapshot("codex", f64::NAN)]
        ),
        Err(LimitCacheError::InvalidPercentage)
    );
    assert_eq!(
        cache.eligibility(
            &account.id,
            /*relevant_limit_id*/ None,
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Eligible
    );
}

#[test]
fn non_chatgpt_accounts_remain_manual_or_pinnable() {
    for (index, mode) in non_chatgpt_auto_selection_modes().into_iter().enumerate() {
        let current = account("current", /*priority*/ 1);
        let selected = account_with_mode(&format!("selected-{index}"), /*priority*/ 2, mode);
        let mut registry = AccountRegistry::default();
        registry.add_account(current.clone()).expect("add current");
        registry
            .add_account(selected.clone())
            .expect("add selected account");
        registry.default_account_id = Some(selected.id.clone());
        let authenticated = HashSet::from([current.id.clone(), selected.id.clone()]);
        let mut cache = AccountLimitCache::default();
        cache
            .update(
                selected.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 1.0)],
            )
            .expect("cache selected account");

        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&current.id), Some(&selected.id), &authenticated),
            )
            .expect("pin selected account"),
            SelectionDecision {
                account_id: selected.id.clone(),
                switched: true,
                reason: SelectionReason::Pinned,
            }
        );
        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(/*current*/ None, /*pinned*/ None, &authenticated)
            )
            .expect("manually select default account"),
            SelectionDecision {
                account_id: selected.id,
                switched: true,
                reason: SelectionReason::ManualDefault,
            }
        );
    }
}

#[test]
fn automatic_failover_only_uses_managed_chatgpt_accounts() {
    let current = account("current", /*priority*/ 10);
    let excluded = non_chatgpt_auto_selection_modes()
        .into_iter()
        .enumerate()
        .map(|(index, mode)| account_with_mode(&format!("excluded-{index}"), index as u32, mode))
        .collect::<Vec<_>>();
    let chatgpt = account("chatgpt", /*priority*/ 20);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add current");
    for account in &excluded {
        registry.add_account(account.clone()).expect("add excluded");
    }
    registry.add_account(chatgpt.clone()).expect("add ChatGPT");
    let authenticated = std::iter::once(current.id.clone())
        .chain(excluded.iter().map(|account| account.id.clone()))
        .chain(std::iter::once(chatgpt.id.clone()))
        .collect::<HashSet<_>>();
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            current.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 100.0)],
        )
        .expect("cache current");
    for account in excluded.iter().chain(std::iter::once(&chatgpt)) {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 1.0)],
            )
            .expect("cache candidate");
    }

    let selected = select_account(
        &registry,
        &cache,
        request(Some(&current.id), /*pinned*/ None, &authenticated),
    )
    .expect("select ChatGPT fallback");
    assert_eq!(selected.account_id, chatgpt.id);

    registry.accounts.retain(|account| account.id != chatgpt.id);
    assert!(matches!(
        select_account(
            &registry,
            &cache,
            request(Some(&current.id), /*pinned*/ None, &authenticated)
        ),
        Err(SelectionError::NoEligibleAccount { .. })
    ));
}

#[test]
fn automatic_selection_never_retains_a_non_chatgpt_current_account() {
    for (index, mode) in non_chatgpt_auto_selection_modes().into_iter().enumerate() {
        let current = account_with_mode(&format!("current-{index}"), /*priority*/ 1, mode);
        let chatgpt = account("chatgpt", /*priority*/ 2);
        let mut registry = AccountRegistry::default();
        registry.auto_selection.enabled = true;
        registry
            .add_account(current.clone())
            .expect("add current account");
        registry
            .add_account(chatgpt.clone())
            .expect("add ChatGPT account");
        let authenticated = HashSet::from([current.id.clone(), chatgpt.id.clone()]);
        let mut cache = AccountLimitCache::default();
        cache
            .update(
                current.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 1.0)],
            )
            .expect("cache current account");
        cache
            .update(
                chatgpt.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 1.0)],
            )
            .expect("cache ChatGPT account");

        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&current.id), /*pinned*/ None, &authenticated)
            )
            .expect("select ChatGPT account"),
            SelectionDecision {
                account_id: chatgpt.id,
                switched: true,
                reason: SelectionReason::CurrentUnsupportedAuth,
            }
        );
    }
}

fn non_chatgpt_auto_selection_modes() -> [AuthMode; 7] {
    [
        AuthMode::ApiKey,
        AuthMode::ChatgptAuthTokens,
        AuthMode::Headers,
        AuthMode::AgentIdentity,
        AuthMode::PersonalAccessToken,
        AuthMode::BedrockApiKey,
        AuthMode::BedrockAccessKeys,
    ]
}

#[test]
fn automatic_candidates_skip_disabled_unauthenticated_and_deleted_accounts() {
    let deleted = AccountId::generate();
    let mut disabled = account("disabled", /*priority*/ 1);
    disabled.enabled = false;
    let unauthenticated = account("unauthenticated", /*priority*/ 2);
    let eligible = account("eligible", /*priority*/ 3);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    for account in [&disabled, &unauthenticated, &eligible] {
        registry.add_account(account.clone()).expect("add account");
    }
    let authenticated = HashSet::from([disabled.id.clone(), eligible.id.clone()]);
    let mut cache = AccountLimitCache::default();
    for account in [&disabled, &unauthenticated, &eligible] {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 1.0)],
            )
            .expect("cache account");
    }

    let decision = select_account(
        &registry,
        &cache,
        request(Some(&deleted), /*pinned*/ None, &authenticated),
    )
    .expect("select remaining candidate");
    assert_eq!(decision.account_id, eligible.id);
    assert_eq!(decision.reason, SelectionReason::NoCurrentAccount);
}

#[test]
fn equal_priority_uses_id_order_without_displacing_an_eligible_current() {
    let first = account("first", /*priority*/ 1_000);
    let second = account("second", /*priority*/ 1_000);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(second.clone()).expect("add second");
    registry.add_account(first.clone()).expect("add first");
    let authenticated = HashSet::from([first.id.clone(), second.id.clone()]);
    let mut cache = AccountLimitCache::default();
    for account in [&first, &second] {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 1.0)],
            )
            .expect("cache account");
    }
    let expected_without_current = std::cmp::min(first.id.clone(), second.id.clone());
    let retained_current = std::cmp::max(first.id, second.id);

    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(/*current*/ None, /*pinned*/ None, &authenticated)
        ),
        Ok(SelectionDecision {
            account_id: expected_without_current,
            switched: true,
            reason: SelectionReason::NoCurrentAccount,
        })
    );
    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(
                Some(&retained_current),
                /*pinned*/ None,
                &authenticated
            ),
        ),
        Ok(SelectionDecision {
            account_id: retained_current,
            switched: false,
            reason: SelectionReason::CurrentEligible,
        })
    );
}

#[test]
fn secondary_reach_and_time_overflow_are_classified() {
    let cached_account = account("cached", /*priority*/ 1);
    let mut cache = AccountLimitCache::default();
    let mut secondary = snapshot("codex", /*used_percent*/ 1.0);
    secondary.secondary = Some(RateLimitWindow {
        used_percent: 100.0,
        window_minutes: Some(300),
        resets_at: Some(2_000),
    });
    cache
        .update(
            cached_account.id.clone(),
            /*observed_at*/ 1_001,
            vec![secondary],
        )
        .expect("cache secondary limit");
    assert_eq!(
        cache.eligibility(
            &cached_account.id,
            /*relevant_limit_id*/ None,
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        Eligibility::Reached(ReachedReason::RateLimit)
    );

    let overflow = account("overflow", /*priority*/ 2);
    cache
        .update(
            overflow.id.clone(),
            i64::MIN,
            vec![snapshot("codex", /*used_percent*/ 1.0)],
        )
        .expect("cache old observation");
    assert_eq!(
        cache.eligibility(
            &overflow.id,
            /*relevant_limit_id*/ None,
            i64::MAX,
            i64::MAX
        ),
        Eligibility::Unknown(UnknownReason::ClockSkew)
    );
}
