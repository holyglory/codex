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
