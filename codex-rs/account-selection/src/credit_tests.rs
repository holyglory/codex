use super::*;
use pretty_assertions::assert_eq;

fn credit_snapshot(used_percent: f64) -> RateLimitSnapshot {
    let mut snapshot = snapshot("codex", used_percent);
    snapshot.credits = Some(CreditsSnapshot {
        has_credits: true,
        unlimited: false,
        balance: Some("10".to_string()),
    });
    snapshot
}

#[test]
fn automatic_credits_follow_included_allowance_within_each_priority() {
    let mut credit = account("credit", /*priority*/ 1_000);
    credit.credit_usage_enabled = true;
    let peer = account("peer", /*priority*/ 1_000);
    let lower = account("lower", /*priority*/ 100);
    let mut registry = AccountRegistry::default();
    registry.auto_selection.enabled = true;
    registry.accounts = vec![credit.clone(), peer.clone(), lower.clone()];
    let authenticated = HashSet::from([credit.id.clone(), peer.id.clone(), lower.id.clone()]);
    let mut cache = AccountLimitCache::default();
    for (account, percent) in [(&credit, 100.0), (&peer, 20.0), (&lower, 10.0)] {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![credit_snapshot(percent)],
            )
            .expect("observe capacity");
    }
    let choose = |registry: &AccountRegistry, cache: &AccountLimitCache| {
        select_account(
            registry,
            cache,
            request(Some(&credit.id), /*pinned*/ None, &authenticated),
        )
        .expect("select known capacity")
    };
    assert_eq!(choose(&registry, &cache).account_id, peer.id);
    cache
        .update(
            peer.id.clone(),
            /*observed_at*/ 1_001,
            vec![credit_snapshot(/*used_percent*/ 100.0)],
        )
        .expect("exhaust peer allowance");
    assert_eq!(
        choose(&registry, &cache),
        SelectionDecision {
            account_id: credit.id.clone(),
            switched: false,
            reason: SelectionReason::CurrentEligible,
        }
    );
    registry.accounts[0].credit_usage_enabled = false;
    assert_eq!(choose(&registry, &cache).account_id, lower.id);
    registry.accounts[0].credit_usage_enabled = true;
    cache.remove(&peer.id);
    assert_eq!(choose(&registry, &cache).account_id, lower.id);
    registry.accounts[1].enabled = false;
    assert_eq!(choose(&registry, &cache).account_id, credit.id);
}

#[test]
fn automatic_credits_require_permission_freshness_and_service_approval() {
    let mut account = account("credit", /*priority*/ 1_000);
    account.credit_usage_enabled = true;
    let mut cache = AccountLimitCache::default();
    let mut observations = vec![(
        credit_snapshot(/*used_percent*/ 100.0),
        AutomaticCapacity::Credits,
    )];
    let mut included = credit_snapshot(/*used_percent*/ 20.0);
    included.credits = Some(CreditsSnapshot {
        has_credits: false,
        unlimited: false,
        balance: Some("0".to_string()),
    });
    observations.push((included, AutomaticCapacity::Included));
    let mut expired = credit_snapshot(/*used_percent*/ 100.0);
    expired.primary.as_mut().expect("primary").resets_at = Some(1_050);
    observations.push((
        expired,
        AutomaticCapacity::Unknown(UnknownReason::InsufficientEvidence),
    ));
    let mut spend = credit_snapshot(/*used_percent*/ 100.0);
    spend.spend_control_reached = Some(true);
    observations.push((
        spend,
        AutomaticCapacity::Reached(ReachedReason::SpendControl),
    ));
    let mut individual = credit_snapshot(/*used_percent*/ 100.0);
    individual.individual_limit = Some(SpendControlLimitSnapshot {
        limit: "100".to_string(),
        used: "100".to_string(),
        remaining_percent: 0,
        resets_at: 2_000,
    });
    observations.push((
        individual,
        AutomaticCapacity::Reached(ReachedReason::SpendControl),
    ));
    for (denial, reason) in [
        (
            RateLimitReachedType::RateLimitReached,
            ReachedReason::RateLimit,
        ),
        (
            RateLimitReachedType::WorkspaceOwnerCreditsDepleted,
            ReachedReason::Credits,
        ),
        (
            RateLimitReachedType::WorkspaceMemberCreditsDepleted,
            ReachedReason::Credits,
        ),
        (
            RateLimitReachedType::WorkspaceOwnerUsageLimitReached,
            ReachedReason::SpendControl,
        ),
        (
            RateLimitReachedType::WorkspaceMemberUsageLimitReached,
            ReachedReason::SpendControl,
        ),
    ] {
        let mut denied = credit_snapshot(/*used_percent*/ 100.0);
        denied.rate_limit_reached_type = Some(denial);
        observations.push((denied, AutomaticCapacity::Reached(reason)));
    }
    for (snapshot, expected) in observations {
        cache
            .update(
                account.id.clone(),
                /*observed_at*/ 1_000,
                vec![snapshot],
            )
            .expect("observe isolated capacity");
        assert_eq!(
            cache.automatic_capacity(
                &account,
                Some("codex"),
                /*now*/ 1_100,
                /*max_age_seconds*/ 300
            ),
            expected
        );
        cache.remove(&account.id);
    }
    let mut credits_only = credit_snapshot(/*used_percent*/ 100.0);
    credits_only.primary = None;
    cache
        .update(
            account.id.clone(),
            /*observed_at*/ 1_000,
            vec![credits_only],
        )
        .expect("observe credits");
    assert_eq!(
        cache.automatic_capacity(
            &account,
            Some("codex"),
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        AutomaticCapacity::Credits
    );
    account.credit_usage_enabled = false;
    assert_eq!(
        cache.automatic_capacity(
            &account,
            Some("codex"),
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        AutomaticCapacity::Reached(ReachedReason::Credits)
    );
    account.credit_usage_enabled = true;
    assert_eq!(
        cache.automatic_capacity(
            &account,
            Some("codex"),
            /*now*/ 1_301,
            /*max_age_seconds*/ 300
        ),
        AutomaticCapacity::Unknown(UnknownReason::Stale)
    );
    assert_eq!(
        cache.automatic_capacity(
            &account,
            Some("auxiliary"),
            /*now*/ 1_100,
            /*max_age_seconds*/ 300
        ),
        AutomaticCapacity::Unknown(UnknownReason::RelevantLimitUnavailable)
    );
}

#[test]
fn credit_permission_preserves_manual_and_pinned_limit_checks() {
    let mut account = account("credit", /*priority*/ 1_000);
    account.credit_usage_enabled = true;
    let authenticated = HashSet::from([account.id.clone()]);
    let mut registry = AccountRegistry::default();
    registry.accounts = vec![account.clone()];
    registry.default_account_id = Some(account.id.clone());
    let mut cache = AccountLimitCache::default();
    cache
        .update(
            account.id.clone(),
            /*observed_at*/ 1_000,
            vec![credit_snapshot(/*used_percent*/ 100.0)],
        )
        .expect("observe exhausted allowance");
    for automatic in [false, true] {
        registry.auto_selection.enabled = automatic;
        assert_eq!(
            select_account(
                &registry,
                &cache,
                request(Some(&account.id), Some(&account.id), &authenticated)
            ),
            Err(SelectionError::LimitReached(ReachedReason::RateLimit))
        );
    }
    registry.auto_selection.enabled = false;
    assert_eq!(
        select_account(
            &registry,
            &cache,
            request(Some(&account.id), /*pinned*/ None, &authenticated)
        ),
        Err(SelectionError::LimitReached(ReachedReason::RateLimit))
    );
}
