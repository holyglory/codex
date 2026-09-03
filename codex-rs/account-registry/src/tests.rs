use std::fs;
use std::process::Command;
use std::time::Duration as StdDuration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::PlanType;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;

fn id(value: u8) -> AccountId {
    format!("acct_{value:032x}")
        .parse()
        .expect("fixed account id should parse")
}

fn timestamp() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")
        .expect("fixed timestamp should parse")
        .with_timezone(&Utc)
}

fn account(id: AccountId, alias: &str, priority: u32) -> AccountMetadata {
    AccountMetadata {
        id,
        alias: alias.parse().expect("fixed alias should parse"),
        auth_mode: AuthMode::Chatgpt,
        email: None,
        plan_type: None,
        enabled: true,
        priority,
        created_at: timestamp(),
        last_used_at: None,
        note: None,
        service_account_id: None,
        service_workspace_id: None,
    }
}

fn registry_with(account: AccountMetadata) -> AccountRegistry {
    AccountRegistry {
        default_account_id: Some(account.id.clone()),
        accounts: vec![account],
        ..AccountRegistry::default()
    }
}

#[test]
fn identifiers_require_canonical_forms() {
    assert_eq!(
        id(/*value*/ 1).as_str(),
        "acct_00000000000000000000000000000001"
    );
    assert_eq!(
        "ACCT_00000000000000000000000000000001".parse::<AccountId>(),
        Err(IdentifierError::InvalidAccountId)
    );
    assert_eq!(
        "Team One".parse::<AccountAlias>(),
        Err(IdentifierError::InvalidAlias)
    );
    assert_eq!(
        "team-one"
            .parse::<AccountAlias>()
            .map(|alias| alias.to_string()),
        Ok("team-one".to_string())
    );
}

#[test]
fn registry_parses_the_versioned_metadata_schema() {
    let json = r#"{
      "version": 1,
      "generation": 12,
      "defaultAccountId": "acct_00000000000000000000000000000001",
      "autoSelection": {"enabled": false, "policy": "priority"},
      "accounts": [{
        "id": "acct_00000000000000000000000000000001",
        "alias": "default",
        "authMode": "chatgpt",
        "email": "example@example.invalid",
        "planType": "unknown",
        "enabled": true,
        "priority": 1,
        "createdAt": "2026-08-20T00:00:00Z",
        "lastUsedAt": null,
        "note": null,
        "serviceAccountId": null,
        "serviceWorkspaceId": null
      }]
    }"#;
    let parsed: AccountRegistry = serde_json::from_str(json).expect("registry should parse");
    let mut expected_account = account(id(/*value*/ 1), "default", /*priority*/ 1);
    expected_account.email = Some("example@example.invalid".to_string());
    expected_account.plan_type = Some(PlanType::Unknown("unknown".to_string()));
    let expected = AccountRegistry {
        generation: 12,
        ..registry_with(expected_account)
    };

    assert_eq!(parsed, expected);
    assert_eq!(parsed.validate(), Ok(()));
}

#[test]
fn registry_rejects_duplicate_ids_and_aliases() {
    let first = account(id(/*value*/ 1), "first", /*priority*/ 1);
    let mut duplicate_id = account(id(/*value*/ 1), "second", /*priority*/ 2);
    let mut registry = registry_with(first.clone());
    assert_eq!(
        registry.add_account(duplicate_id.clone()),
        Err(RegistryValidationError::DuplicateId {
            id: id(/*value*/ 1)
        })
    );

    duplicate_id.id = id(/*value*/ 2);
    duplicate_id.alias = first.alias.clone();
    assert_eq!(
        registry.add_account(duplicate_id),
        Err(RegistryValidationError::DuplicateAlias { alias: first.alias })
    );
}

#[test]
fn exact_lookup_reports_unknown_ambiguous_and_disabled_accounts() {
    let second_id = id(/*value*/ 2);
    let alias_collision = account(id(/*value*/ 1), second_id.as_str(), /*priority*/ 1);
    let second = account(second_id.clone(), "second", /*priority*/ 2);
    let mut disabled = account(id(/*value*/ 3), "disabled", /*priority*/ 3);
    disabled.enabled = false;
    let registry = AccountRegistry {
        accounts: vec![alias_collision, second, disabled.clone()],
        ..AccountRegistry::default()
    };

    assert_eq!(
        registry.lookup(second_id.as_str()),
        Err(AccountLookupError::Ambiguous {
            reference: second_id.to_string()
        })
    );
    assert_eq!(
        registry.lookup("missing"),
        Err(AccountLookupError::Unknown {
            reference: "missing".to_string()
        })
    );
    assert_eq!(
        registry.lookup("disabled"),
        Err(AccountLookupError::Disabled {
            id: disabled.id,
            alias: disabled.alias
        })
    );
}

#[test]
fn exact_lookup_succeeds_for_a_complete_alias_or_id_but_not_a_prefix() {
    let first = account(id(/*value*/ 1), "first", /*priority*/ 1);
    let registry = registry_with(first.clone());

    assert_eq!(registry.lookup("first"), Ok(&first));
    assert_eq!(registry.lookup(first.id.as_str()), Ok(&first));
    assert_eq!(
        registry.lookup("fir"),
        Err(AccountLookupError::Unknown {
            reference: "fir".to_string()
        })
    );
}

#[test]
fn only_a_complete_duplicate_service_identity_is_rejected() {
    let service_account = OpaqueServiceId::new("service-account").expect("service id");
    let workspace = OpaqueServiceId::new("workspace").expect("workspace id");
    let mut first = account(id(/*value*/ 1), "first", /*priority*/ 1);
    first.service_account_id = Some(service_account.clone());
    first.service_workspace_id = Some(workspace.clone());
    let mut registry = registry_with(first);

    let mut different_workspace =
        account(id(/*value*/ 2), "different-workspace", /*priority*/ 2);
    different_workspace.service_account_id = Some(service_account.clone());
    different_workspace.service_workspace_id =
        Some(OpaqueServiceId::new("workspace-two").expect("workspace id"));
    registry
        .add_account(different_workspace)
        .expect("one shared field is insufficient");

    let mut partial = account(id(/*value*/ 3), "partial", /*priority*/ 3);
    partial.service_account_id = Some(service_account.clone());
    registry
        .add_account(partial)
        .expect("a partial service identity is insufficient");

    let mut duplicate = account(id(/*value*/ 4), "duplicate", /*priority*/ 4);
    duplicate.service_account_id = Some(service_account);
    duplicate.service_workspace_id = Some(workspace);
    let expected = RegistryValidationError::DuplicateServiceIdentity {
        first_id: id(/*value*/ 1),
        duplicate_id: id(/*value*/ 4),
    };
    let mut loaded = registry.clone();
    loaded.accounts.push(duplicate.clone());
    assert_eq!(loaded.validate(), Err(expected.clone()));
    assert_eq!(registry.add_account(duplicate), Err(expected));
}

#[test]
fn enabled_accounts_sort_by_descending_priority_then_id() {
    let mut disabled = account(id(/*value*/ 4), "disabled", /*priority*/ 0);
    disabled.enabled = false;
    let registry = AccountRegistry {
        auto_selection: AutoSelection {
            enabled: true,
            policy: SelectionPolicy::Priority,
        },
        accounts: vec![
            account(id(/*value*/ 3), "third", /*priority*/ 2),
            account(id(/*value*/ 2), "second", /*priority*/ 1),
            account(id(/*value*/ 1), "first", /*priority*/ 1),
            disabled,
        ],
        ..AccountRegistry::default()
    };

    assert_eq!(
        registry
            .enabled_by_priority()
            .into_iter()
            .map(|account| account.id.clone())
            .collect::<Vec<_>>(),
        vec![id(/*value*/ 3), id(/*value*/ 1), id(/*value*/ 2)]
    );
}

#[test]
fn protected_service_ids_are_redacted_from_debug_output() {
    let secret = "service-account-opaque-value";
    let mut metadata = account(id(/*value*/ 1), "default", /*priority*/ 1);
    metadata.service_account_id = Some(OpaqueServiceId::new(secret).expect("valid service id"));

    let debug = format!("{metadata:?}");
    assert!(!debug.contains(secret));
    assert!(debug.contains("[redacted]"));
}

#[test]
fn registry_store_debug_does_not_expose_its_codex_home() {
    let secret_path = "/private/operator/codex-home";
    let store = RegistryStore::new(std::path::Path::new(secret_path));

    let debug = format!("{store:?}");
    assert!(!debug.contains(secret_path));
    assert_eq!(debug, "RegistryStore { .. }");
}

#[test]
fn compare_and_swap_preserves_the_last_valid_registry() {
    let directory = tempdir().expect("create temp directory");
    let store = RegistryStore::new(directory.path());
    store
        .create(&registry_with(account(
            id(/*value*/ 1),
            "first",
            /*priority*/ 1,
        )))
        .expect("create registry");
    let updated = store
        .compare_and_swap(/*expected_generation*/ 0, |registry| {
            registry
                .add_account(account(id(/*value*/ 2), "second", /*priority*/ 2))
                .expect("unique account");
        })
        .expect("update registry");
    assert_eq!(updated.generation, 1);

    assert!(matches!(
        store.compare_and_swap(/*expected_generation*/ 0, |_| {}),
        Err(RegistryStoreError::GenerationConflict {
            expected: 0,
            actual: 1
        })
    ));
    let before = fs::read(store.registry_path()).expect("read registry bytes");
    assert!(matches!(
        store.compare_and_swap(/*expected_generation*/ 1, |registry| {
            registry.accounts[1].alias = registry.accounts[0].alias.clone();
        }),
        Err(RegistryStoreError::Validation(
            RegistryValidationError::DuplicateAlias { .. }
        ))
    ));
    assert_eq!(
        fs::read(store.registry_path()).expect("read preserved registry bytes"),
        before
    );
    assert_eq!(store.read().expect("read registry"), updated);
}

#[test]
fn advisory_lock_excludes_independent_handles_and_recovers_on_drop() {
    let directory = tempdir().expect("create temp directory");
    let first_store = RegistryStore::new(directory.path());
    let second_store = RegistryStore::new(directory.path());
    let first = first_store.acquire_lock().expect("acquire first lock");
    assert!(matches!(
        second_store.try_acquire_lock(),
        Err(RegistryStoreError::LockBusy)
    ));
    drop(first);
    second_store
        .try_acquire_lock()
        .expect("lock should recover when the owner closes it");
}

#[test]
fn guard_bound_mutations_do_not_reacquire_and_reject_another_store_guard() {
    let first_directory = tempdir().expect("create first temp directory");
    let second_directory = tempdir().expect("create second temp directory");
    let first_store = RegistryStore::new(first_directory.path());
    let first_clone = first_store.clone();
    let second_store = RegistryStore::new(second_directory.path());
    let guard = first_store.acquire_lock().expect("acquire guard");

    first_clone
        .create_with_guard(
            &guard,
            &registry_with(account(id(/*value*/ 1), "first", /*priority*/ 1)),
        )
        .expect("guard-bound create should not reacquire");
    let updated = first_store
        .compare_and_swap_with_guard(&guard, /*expected_generation*/ 0, |registry| {
            registry.accounts[0].note = Some("updated".to_string());
        })
        .expect("guard-bound CAS should not reacquire");
    assert_eq!(updated.generation, 1);
    assert!(matches!(
        second_store.create_with_guard(&guard, &AccountRegistry::default()),
        Err(RegistryStoreError::GuardMismatch)
    ));
    assert!(matches!(
        second_store.repair_committed_durability_with_guard(&guard),
        Err(RegistryStoreError::GuardMismatch)
    ));
    first_store
        .repair_committed_durability_with_guard(&guard)
        .expect("guard-bound parent sync repair should succeed");
    assert_eq!(first_store.read().expect("read repaired registry"), updated);
}

#[test]
fn committed_directory_sync_failure_remains_explicit_and_redacted() {
    let error = crate::store::committed_directory_sync_result(Err(std::io::Error::other(
        "simulated parent sync failure",
    )))
    .expect_err("sync failure should remain uncertain");

    assert!(matches!(
        &error,
        RegistryStoreError::CommittedDurabilityUncertain { .. }
    ));
    assert_eq!(
        error.to_string(),
        "account registry update committed, but durability is uncertain while attempting to synchronize accounts directory: simulated parent sync failure"
    );
}

#[test]
fn interrupted_guarded_mutation_preserves_registry_and_releases_the_lock() {
    let directory = tempdir().expect("create temp directory");
    let store = RegistryStore::new(directory.path());
    let original = registry_with(account(id(/*value*/ 1), "first", /*priority*/ 1));
    store.create(&original).expect("create registry");
    let guard = store.acquire_lock().expect("acquire guard");

    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.compare_and_swap_with_guard(&guard, /*expected_generation*/ 0, |_| {
            panic!("simulated interrupted mutation");
        });
    }));
    assert!(interrupted.is_err());
    assert_eq!(store.read().expect("read preserved registry"), original);
    drop(guard);
    store
        .try_acquire_lock()
        .expect("lock should be available after interrupted owner drops");
}

#[cfg(unix)]
#[test]
fn failed_atomic_rename_removes_the_temporary_registry() {
    let directory = tempdir().expect("create temp directory");
    let store = RegistryStore::new(directory.path());
    let original = registry_with(account(id(/*value*/ 1), "first", /*priority*/ 1));
    store.create(&original).expect("create registry");
    let guard = store.acquire_lock().expect("acquire guard");
    let index_path = store.registry_path().to_path_buf();
    let backup_path = index_path.with_extension("backup");

    let error = store
        .compare_and_swap_with_guard(&guard, /*expected_generation*/ 0, |_| {
            fs::rename(&index_path, &backup_path).expect("preserve original registry");
            fs::create_dir(&index_path).expect("make rename destination invalid");
        })
        .expect_err("replacement over a directory should fail");
    assert!(matches!(error, RegistryStoreError::Io { .. }));
    let entries = fs::read_dir(directory.path().join("accounts"))
        .expect("read accounts directory")
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .expect("read every account directory entry");
    assert!(
        entries
            .iter()
            .all(|name| !name.to_string_lossy().starts_with(".private-"))
    );

    fs::remove_dir(&index_path).expect("remove invalid destination");
    fs::rename(&backup_path, &index_path).expect("restore original registry");
    assert_eq!(store.read().expect("read restored registry"), original);
}

const CAS_WRITER_HELPER_ROOT: &str = "CODEX_ACCOUNT_REGISTRY_CAS_WRITER_HELPER_ROOT";
const CAS_WRITER_HELPER_NOTE: &str = "CODEX_ACCOUNT_REGISTRY_CAS_WRITER_HELPER_NOTE";

#[test]
fn concurrent_cas_writers_cannot_lose_an_update() {
    match (
        std::env::var_os(CAS_WRITER_HELPER_ROOT),
        std::env::var_os(CAS_WRITER_HELPER_NOTE),
    ) {
        (Some(root), Some(note)) => {
            let root = std::path::PathBuf::from(root);
            let note = note.into_string().expect("writer note should be UTF-8");
            assert!(matches!(note.as_str(), "one" | "two"));
            fs::write(root.join(format!("ready-{note}")), b"ready")
                .expect("signal CAS writer readiness");
            wait_for_path(&root.join("start"));
            let store = RegistryStore::new(&root);
            let outcome = match store.compare_and_swap(/*expected_generation*/ 0, |registry| {
                registry.accounts[0].note = Some(note.clone());
            }) {
                Ok(_) => "updated",
                Err(RegistryStoreError::GenerationConflict {
                    expected: 0,
                    actual: 1,
                }) => "conflict",
                Err(error) => panic!("unexpected CAS writer failure: {error}"),
            };
            fs::write(root.join(format!("result-{note}")), outcome)
                .expect("record CAS writer outcome");
            return;
        }
        (None, None) => {}
        _ => panic!("CAS writer helper environment must be complete"),
    }

    let directory = tempdir().expect("create temp directory");
    let store = RegistryStore::new(directory.path());
    store
        .create(&registry_with(account(
            id(/*value*/ 1),
            "first",
            /*priority*/ 1,
        )))
        .expect("create registry");
    let mut children = ["one", "two"].map(|note| {
        Command::new(std::env::current_exe().expect("resolve current test executable"))
            .args([
                "--exact",
                "tests::concurrent_cas_writers_cannot_lose_an_update",
                "--nocapture",
            ])
            .env(CAS_WRITER_HELPER_ROOT, directory.path())
            .env(CAS_WRITER_HELPER_NOTE, note)
            .spawn()
            .expect("spawn CAS writer helper")
    });
    for note in ["one", "two"] {
        wait_for_path(&directory.path().join(format!("ready-{note}")));
    }
    fs::write(directory.path().join("start"), b"start").expect("release CAS writers");
    for child in &mut children {
        assert!(wait_for_child(child).success(), "CAS writer should succeed");
    }
    let mut outcomes = ["one", "two"]
        .map(|note| {
            fs::read_to_string(directory.path().join(format!("result-{note}")))
                .expect("read CAS writer outcome")
        })
        .to_vec();
    outcomes.sort();

    assert_eq!(
        outcomes,
        vec!["conflict".to_string(), "updated".to_string()]
    );
    assert_eq!(store.read().expect("read registry").generation, 1);
}

#[cfg(unix)]
const PROCESS_LOCK_HELPER_ROOT: &str = "CODEX_ACCOUNT_REGISTRY_PROCESS_LOCK_HELPER_ROOT";

#[cfg(unix)]
#[test]
fn process_lock_helper() {
    let Some(root) = std::env::var_os(PROCESS_LOCK_HELPER_ROOT) else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let store = RegistryStore::new(&root);
    let _guard = store.acquire_lock().expect("child should acquire lock");
    fs::write(root.join("ready"), b"ready").expect("signal lock acquisition");
    wait_for_path(&root.join("release"));
    std::process::exit(0);
}

#[cfg(unix)]
#[test]
fn process_exit_releases_the_registry_lock() {
    let directory = tempdir().expect("create temp directory");
    let mut child = Command::new(std::env::current_exe().expect("resolve current test executable"))
        .args(["--exact", "tests::process_lock_helper", "--nocapture"])
        .env(PROCESS_LOCK_HELPER_ROOT, directory.path())
        .spawn()
        .expect("spawn lock helper");
    wait_for_path(&directory.path().join("ready"));

    let store = RegistryStore::new(directory.path());
    assert!(matches!(
        store.try_acquire_lock(),
        Err(RegistryStoreError::LockBusy)
    ));
    fs::write(directory.path().join("release"), b"release").expect("release helper");
    assert!(wait_for_child(&mut child).success());
    store
        .try_acquire_lock()
        .expect("kernel should release an abandoned process lock");
}

fn wait_for_path(path: &std::path::Path) {
    let deadline = Instant::now() + StdDuration::from_secs(/*secs*/ 5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        std::thread::sleep(StdDuration::from_millis(/*millis*/ 10));
    }
}

fn wait_for_child(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + StdDuration::from_secs(/*secs*/ 5);
    loop {
        if let Some(status) = child.try_wait().expect("inspect helper process") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for helper process");
        }
        std::thread::sleep(StdDuration::from_millis(/*millis*/ 10));
    }
}

#[cfg(unix)]
#[test]
fn store_uses_private_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().expect("create temp directory");
    let store = RegistryStore::new(directory.path());
    store
        .create(&registry_with(account(
            id(/*value*/ 1),
            "first",
            /*priority*/ 1,
        )))
        .expect("create registry");

    assert_eq!(
        fs::metadata(directory.path().join("accounts"))
            .expect("accounts metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(store.registry_path())
            .expect("registry metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(windows)]
#[test]
fn store_supports_private_windows_workflow() {
    let directory = tempdir().expect("create temp directory");
    let store = RegistryStore::new(directory.path());
    store
        .create(&registry_with(account(id(1), "first", 1)))
        .expect("create registry");
    let updated = store
        .compare_and_swap(0, |registry| {
            registry.accounts[0].note = Some("updated".to_string());
        })
        .expect("replace registry");
    assert_eq!(store.read().expect("read registry"), updated);
    codex_private_storage::verify_private_directory(&directory.path().join("accounts"))
        .expect("private accounts directory");
    codex_private_storage::verify_private_file(store.registry_path())
        .expect("private registry file");
}

#[test]
fn migration_journal_enforces_forward_and_rollback_transitions() {
    let now = timestamp();
    let mut journal = LegacyMigrationJournal::new(id(/*value*/ 1), now);
    assert_eq!(
        journal.transition(LegacyMigrationStage::Completed, now + Duration::seconds(2)),
        Err(MigrationJournalError::InvalidTransition {
            from: LegacyMigrationStage::Prepared,
            to: LegacyMigrationStage::Completed,
        })
    );
    assert_eq!(
        journal.transition(
            LegacyMigrationStage::LegacyBackupPreserved,
            now - Duration::seconds(1),
        ),
        Err(MigrationJournalError::TimestampRegression)
    );
    for (stage, seconds) in [
        (LegacyMigrationStage::LegacyBackupPreserved, 1),
        (LegacyMigrationStage::CredentialStored, 2),
        (LegacyMigrationStage::Verified, 3),
        (LegacyMigrationStage::RegistryStored, 4),
        (LegacyMigrationStage::Completed, 5),
    ] {
        journal
            .transition(stage, now + Duration::seconds(seconds))
            .expect("advance migration in order");
    }
    assert_eq!(journal.stage(), LegacyMigrationStage::Completed);
    assert_eq!(
        journal.rollback(now + Duration::seconds(6)),
        Err(MigrationJournalError::InvalidTransition {
            from: LegacyMigrationStage::Completed,
            to: LegacyMigrationStage::RolledBack,
        })
    );

    let mut rollback = LegacyMigrationJournal::new(id(/*value*/ 2), now);
    rollback
        .transition(
            LegacyMigrationStage::LegacyBackupPreserved,
            now + Duration::seconds(1),
        )
        .expect("preserve backup");
    rollback
        .rollback(now + Duration::seconds(2))
        .expect("controlled rollback");
    assert_eq!(rollback.stage(), LegacyMigrationStage::RolledBack);
    assert_eq!(
        rollback.transition(
            LegacyMigrationStage::CredentialStored,
            now + Duration::seconds(3),
        ),
        Err(MigrationJournalError::InvalidTransition {
            from: LegacyMigrationStage::RolledBack,
            to: LegacyMigrationStage::CredentialStored,
        })
    );
}

#[test]
fn migration_journal_deserialization_rejects_version_and_timestamp_corruption() {
    let now = timestamp();
    let journal = LegacyMigrationJournal::new(id(/*value*/ 1), now);
    let mut unsupported = serde_json::to_value(&journal).expect("serialize journal");
    unsupported["version"] = serde_json::json!(2);
    assert!(
        serde_json::from_value::<LegacyMigrationJournal>(unsupported)
            .expect_err("unsupported version should fail")
            .to_string()
            .contains("unsupported legacy migration journal version")
    );

    let mut reversed = serde_json::to_value(journal).expect("serialize journal");
    reversed["updatedAt"] = serde_json::json!("2026-08-19T23:59:59Z");
    assert!(
        serde_json::from_value::<LegacyMigrationJournal>(reversed)
            .expect_err("reversed timestamps should fail")
            .to_string()
            .contains("precedes its start time")
    );
}
