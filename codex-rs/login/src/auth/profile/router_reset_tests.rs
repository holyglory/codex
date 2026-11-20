use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
#[serial(codex_auth_env)]
async fn selection_probe_compares_all_peers_then_rechecks_resets_on_the_next_turn() {
    let home = tempdir().expect("home");
    let current = seed_chatgpt_account(home.path(), "current");
    let mut peers = [
        seed_chatgpt_account(home.path(), "peer-a"),
        seed_chatgpt_account(home.path(), "peer-b"),
    ];
    peers.sort_by(|left, right| left.id.cmp(&right.id));
    let [earlier, later] = peers;
    let mut lower = seed_chatgpt_account(home.path(), "lower");
    lower.priority = 1;
    seed_registry(
        home.path(),
        vec![current.clone(), earlier.clone(), later.clone(), lower],
        /*default*/ 0,
    );
    RegistryStore::new(home.path())
        .compare_and_swap(
            /*expected_generation*/ 0,
            |registry| registry.auto_selection.enabled = true,
        )
        .expect("enable auto selection");
    let shared = SharedProfileAuthRouter::new_with_external_auth(
        config(home.path()),
        RouterExternalAuthState::default(),
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("upstream")),
    );
    shared
        .record_rate_limits(
            current.id.clone(),
            Utc::now().timestamp(),
            vec![limits(/*used_percent*/ 10.0)],
        )
        .expect("cache current");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_for_probe = Arc::clone(&calls);
    let later_id = later.id.clone();
    let first = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            move |lease| {
                let id = lease.account_id().expect("profile lease").clone();
                calls_for_probe.lock().expect("calls").push(id.clone());
                let mut snapshot = limits(/*used_percent*/ 10.0);
                snapshot.primary.as_mut().expect("primary").resets_at =
                    Some(Utc::now().timestamp() + if id == later_id { 3_600 } else { 1_800 });
                async move { Some(vec![snapshot]) }
            },
        )
        .await
        .expect("selection")
        .expect("first lease");
    assert_eq!(first.account_id(), &later.id);
    assert!(first.automatic_switched());
    assert_eq!(
        *calls.lock().expect("calls"),
        vec![earlier.id, later.id.clone()]
    );

    shared.remove_rate_limits(&current.id);
    let mut updated = limits(/*used_percent*/ 10.0);
    updated.primary.as_mut().expect("primary").resets_at = Some(Utc::now().timestamp() + 7_200);
    shared
        .record_rate_limits(current.id.clone(), Utc::now().timestamp(), vec![updated])
        .expect("cache updated reset");
    let second = shared
        .lease_for_turn_with_external_auth_and_probe(
            RouterExternalAuthState::default(),
            |_lease| async { panic!("fresh winning-tier peers do not need probes") },
        )
        .await
        .expect("next selection")
        .expect("second lease");
    assert_eq!(second.account_id(), &current.id);
    assert!(second.automatic_switched());
    assert_eq!(first.account_id(), &later.id);
}
