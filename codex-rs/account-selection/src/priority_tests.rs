use super::*;
use pretty_assertions::assert_eq;

#[test]
fn equal_priority_requires_at_least_ten_minutes_of_reset_advantage() {
    let current = account("current", /*priority*/ 1_000);
    let peer = account("peer", /*priority*/ 1_000);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add current");
    registry.add_account(peer.clone()).expect("add peer");
    let authenticated = HashSet::from([current.id.clone(), peer.id.clone()]);

    for (current_reset, peer_reset, expected) in [
        (Some(2_000), Some(3_000), &peer.id),
        (Some(3_000), Some(2_000), &current.id),
        (Some(2_000), Some(2_000), &current.id),
        (Some(2_000), Some(2_599), &current.id),
        (Some(2_000), Some(2_600), &peer.id),
        (Some(2_000), Some(2_601), &peer.id),
        (None, Some(3_000), &peer.id),
        (Some(2_000), None, &current.id),
        (None, None, &current.id),
        (Some(2_000), Some(i64::MAX), &peer.id),
    ] {
        let mut cache = AccountLimitCache::default();
        for (account, reset) in [(&current, current_reset), (&peer, peer_reset)] {
            let mut limits = snapshot("codex", /*used_percent*/ 10.0);
            limits.primary.as_mut().expect("primary").resets_at = reset;
            cache
                .update(account.id.clone(), /*observed_at*/ 1_000, vec![limits])
                .expect("cache limits");
        }
        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&current.id), /*pinned*/ None, &authenticated),
            ),
            Ok(SelectionDecision {
                account_id: expected.clone(),
                switched: expected != &current.id,
                reason: SelectionReason::CurrentEligible,
            }),
            "current reset {current_reset:?}, peer reset {peer_reset:?}"
        );
    }
}

#[test]
fn reset_ranking_uses_the_next_used_main_quota_window() {
    let current = account("current", /*priority*/ 1_000);
    let peer = account("peer", /*priority*/ 1_000);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add current");
    registry.add_account(peer.clone()).expect("add peer");
    let authenticated = HashSet::from([current.id.clone(), peer.id.clone()]);

    for (primary_used, primary_reset, expected) in [
        (0.0, Some(1_500), &peer.id),
        (10.0, Some(1_500), &current.id),
        (10.0, None, &peer.id),
    ] {
        let mut main = snapshot("codex", primary_used);
        main.primary.as_mut().expect("primary").resets_at = primary_reset;
        main.secondary = Some(RateLimitWindow {
            used_percent: 20.0,
            window_minutes: Some(10_080),
            resets_at: Some(3_000),
        });
        let mut auxiliary = snapshot("spark", /*used_percent*/ 25.0);
        auxiliary.primary.as_mut().expect("primary").resets_at = Some(9_000);
        let mut cache = AccountLimitCache::default();
        cache
            .update(
                current.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot("codex", /*used_percent*/ 10.0), auxiliary],
            )
            .expect("cache current");
        cache
            .update(peer.id.clone(), /*observed_at*/ 1_000, vec![main])
            .expect("cache peer");
        let mut selection = request(Some(&current.id), /*pinned*/ None, &authenticated);
        selection.relevant_limit_id = None;
        assert_eq!(
            select_account(&registry, &cache, selection),
            Ok(SelectionDecision {
                account_id: expected.clone(),
                switched: expected != &current.id,
                reason: SelectionReason::CurrentEligible,
            })
        );
    }
}

#[test]
fn later_resets_do_not_override_priority_eligibility_or_pins() {
    let current = account("current", /*priority*/ 2_000);
    let peer = account("peer", /*priority*/ 1_000);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.add_account(current.clone()).expect("add current");
    registry.add_account(peer.clone()).expect("add peer");
    let authenticated = HashSet::from([current.id.clone(), peer.id.clone()]);
    let mut cache = AccountLimitCache::default();
    let mut peer_limits = snapshot("codex", /*used_percent*/ 10.0);
    peer_limits.primary.as_mut().expect("primary").resets_at = Some(3_000);
    cache
        .update(
            current.id.clone(),
            /*observed_at*/ 1_000,
            vec![snapshot("codex", /*used_percent*/ 10.0)],
        )
        .expect("cache current");
    cache
        .update(
            peer.id.clone(),
            /*observed_at*/ 1_000,
            vec![peer_limits],
        )
        .expect("cache peer");
    let selection = request(Some(&current.id), /*pinned*/ None, &authenticated);
    assert_eq!(
        select_account(&registry, &cache, selection),
        Ok(SelectionDecision {
            account_id: current.id.clone(),
            switched: false,
            reason: SelectionReason::CurrentEligible,
        })
    );
    registry.accounts[1].priority = current.priority;
    let mut higher = registry.clone();
    higher.accounts[1].priority += 1;
    cache.remove(&peer.id);
    let mut peer_limits = snapshot("codex", /*used_percent*/ 10.0);
    peer_limits.primary.as_mut().expect("primary").resets_at = Some(2_599);
    cache
        .update(
            peer.id.clone(),
            /*observed_at*/ 1_000,
            vec![peer_limits],
        )
        .expect("cache peer within margin");
    assert_eq!(
        select_account(
            &higher,
            &cache,
            request(Some(&current.id), /*pinned*/ None, &authenticated),
        ),
        Ok(SelectionDecision {
            account_id: peer.id.clone(),
            switched: true,
            reason: SelectionReason::CurrentEligible,
        })
    );
    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(Some(&current.id), Some(&current.id), &authenticated),
        ),
        Ok(SelectionDecision {
            account_id: current.id.clone(),
            switched: false,
            reason: SelectionReason::Pinned,
        })
    );
    for observed_at in [700, 1_000] {
        cache.remove(&peer.id);
        let mut limits = snapshot("codex", /*used_percent*/ 100.0);
        limits.primary.as_mut().expect("primary").resets_at = Some(3_000);
        cache
            .update(peer.id.clone(), observed_at, vec![limits])
            .expect("cache unavailable peer");
        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&current.id), /*pinned*/ None, &authenticated),
            ),
            Ok(SelectionDecision {
                account_id: current.id.clone(),
                switched: false,
                reason: SelectionReason::CurrentEligible,
            })
        );
    }
}

#[test]
fn credit_fallback_returns_to_included_usage_and_never_overrides_service_denials() {
    let mut credit = account("credit", /*priority*/ 2000);
    credit.credit_usage_enabled = true;
    let peer = account("peer", /*priority*/ 2000);
    let lower = account("lower", /*priority*/ 1000);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.accounts = vec![credit.clone(), peer.clone(), lower.clone()];
    let authenticated = registry
        .accounts
        .iter()
        .map(|account| account.id.clone())
        .collect::<HashSet<_>>();
    let mut cache = AccountLimitCache::default();
    let mut credit_limits = snapshot("codex", /*used_percent*/ 100.0);
    credit_limits.credits = Some(CreditsSnapshot {
        has_credits: true,
        unlimited: false,
        balance: Some("9.99".to_string()),
    });
    cache
        .update(
            credit.id.clone(),
            /*observed_at*/ 1000,
            vec![credit_limits.clone()],
        )
        .unwrap();
    cache
        .update(
            lower.id.clone(),
            /*observed_at*/ 1000,
            vec![snapshot("codex", /*used_percent*/ 1.0)],
        )
        .unwrap();
    // Unknown peers must not authorize spending, even with a fresh credit balance.
    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(Some(&credit.id), None, &authenticated)
        )
        .unwrap()
        .account_id,
        lower.id
    );
    for (observed, used, expected) in [(1001, 100.0, &credit.id), (1002, 10.0, &peer.id)] {
        cache
            .update(peer.id.clone(), observed, vec![snapshot("codex", used)])
            .unwrap();
        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&credit.id), None, &authenticated)
            )
            .unwrap()
            .account_id,
            *expected
        );
    }
    cache
        .update(
            peer.id,
            /*observed_at*/ 1003,
            vec![snapshot("codex", /*used_percent*/ 100.0)],
        )
        .unwrap();
    for reached in [
        RateLimitReachedType::RateLimitReached,
        RateLimitReachedType::WorkspaceOwnerCreditsDepleted,
        RateLimitReachedType::WorkspaceMemberCreditsDepleted,
        RateLimitReachedType::WorkspaceOwnerUsageLimitReached,
        RateLimitReachedType::WorkspaceMemberUsageLimitReached,
    ] {
        let mut denied = credit_limits.clone();
        denied.rate_limit_reached_type = Some(reached);
        cache.remove(&credit.id);
        cache
            .update(credit.id.clone(), /*observed_at*/ 1000, vec![denied])
            .unwrap();
        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&credit.id), None, &authenticated)
            )
            .unwrap()
            .account_id,
            lower.id
        );
    }
    credit_limits.individual_limit = Some(SpendControlLimitSnapshot {
        limit: "10".to_string(),
        used: "10".to_string(),
        remaining_percent: 0,
        resets_at: 2000,
    });
    cache.remove(&credit.id);
    cache
        .update(
            credit.id.clone(),
            /*observed_at*/ 1000,
            vec![credit_limits],
        )
        .unwrap();
    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(Some(&credit.id), None, &authenticated)
        )
        .unwrap()
        .account_id,
        lower.id
    );
}

#[test]
fn automatic_credit_capacity_requires_fresh_permission_and_available_credits() {
    let mut profile = account("credits", /*priority*/ 1000);
    for (enabled, has_credits, unlimited, observed_at, expected) in [
        (
            false,
            true,
            false,
            1000,
            AutomaticCapacity::Reached(ReachedReason::RateLimit),
        ),
        (true, true, false, 1000, AutomaticCapacity::Credits),
        (true, false, true, 1000, AutomaticCapacity::Credits),
        (
            true,
            false,
            false,
            1000,
            AutomaticCapacity::Reached(ReachedReason::RateLimit),
        ),
        (
            true,
            true,
            false,
            700,
            AutomaticCapacity::Unknown(UnknownReason::Stale),
        ),
    ] {
        profile.credit_usage_enabled = enabled;
        let mut limits = snapshot("codex", /*used_percent*/ 10.0);
        limits.secondary = Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(10080),
            resets_at: Some(2000),
        });
        limits.credits = Some(CreditsSnapshot {
            has_credits,
            unlimited,
            balance: None,
        });
        let mut cache = AccountLimitCache::default();
        cache
            .update(profile.id.clone(), observed_at, vec![limits])
            .unwrap();
        assert_eq!(
            cache.automatic_capacity(
                &profile, None, /*now*/ 1100, /*max_age_seconds*/ 300
            ),
            expected
        );
    }
}
