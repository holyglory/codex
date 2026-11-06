//! Historical keys are seeded independently of the storage constructors under test.

use super::*;
use codex_keyring_store::CredentialStoreError;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[derive(Debug)]
struct HistoricalKeyring {
    values: BTreeMap<(String, String), Option<String>>,
}

impl KeyringStore for HistoricalKeyring {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        Ok(self
            .values
            .get(&(service.to_string(), account.to_string()))
            .expect("only independently seeded historical credential names may be read")
            .clone())
    }

    fn save(
        &self,
        _service: &str,
        _account: &str,
        _value: &str,
    ) -> Result<(), CredentialStoreError> {
        panic!("reading preserved credentials must not create a replacement key")
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<bool, CredentialStoreError> {
        panic!("reading preserved credentials must not delete a historical key")
    }
}

fn historical_suffix(home: &Path) -> anyhow::Result<String> {
    let canonical = home.canonicalize()?;
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    Ok(format!("{digest:x}")[..16].to_string())
}

#[test]
fn profile_direct_keyring_reads_saved_v1_record_without_legacy_fallback() -> anyhow::Result<()> {
    let home = tempdir()?;
    let suffix = historical_suffix(home.path())?;
    let saved = r#"{"OPENAI_API_KEY":"synthetic-preserved-profile"}"#;
    let legacy = r#"{"OPENAI_API_KEY":"synthetic-legacy-record"}"#;
    for present in [true, false] {
        let keyring: Arc<dyn KeyringStore> = Arc::new(HistoricalKeyring {
            values: BTreeMap::from([
                (
                    ("Codex Auth".to_string(), format!("cli|profile-v1|{suffix}")),
                    present.then(|| saved.to_string()),
                ),
                (
                    ("Codex Auth".to_string(), format!("cli|{suffix}")),
                    Some(legacy.to_string()),
                ),
            ]),
        });
        for mode in [
            AuthCredentialsStoreMode::Keyring,
            AuthCredentialsStoreMode::Auto,
        ] {
            let storage = create_auth_storage_with_store_and_namespace(
                home.path().to_path_buf(),
                mode,
                Arc::clone(&keyring),
                AuthKeyringBackendKind::Direct,
                AuthStorageNamespace::ProfileV1,
            );
            let expected = present
                .then(|| serde_json::from_str::<AuthDotJson>(saved).expect("historical auth"));
            assert_eq!(storage.load()?, expected);
            let guard = storage.acquire_lock()?;
            assert_eq!(
                storage.resolved_backend_kind_with_guard(&guard)?,
                present.then_some(PersistentAuthBackendKind::DirectKeyring)
            );
            assert_eq!(
                storage.backend().backend_kind(),
                PersistentAuthBackendKind::DirectKeyring
            );
        }
    }
    Ok(())
}

#[test]
fn profile_secrets_keyring_uses_historical_passphrase_namespace() -> anyhow::Result<()> {
    let home = tempdir()?;
    let key = format!(
        "secrets|codex-profile-auth-v1|{}",
        historical_suffix(home.path())?
    );
    let keyring: Arc<dyn KeyringStore> = Arc::new(HistoricalKeyring {
        values: BTreeMap::from([(
            ("codex".to_string(), key),
            Some("synthetic historical profile passphrase".to_string()),
        )]),
    });
    let saved = r#"{"OPENAI_API_KEY":"synthetic-encrypted-profile"}"#;
    let expected: AuthDotJson = serde_json::from_str(saved)?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        home.path().to_path_buf(),
        SecretsBackendKind::Local,
        Arc::clone(&keyring),
        LocalSecretsNamespace::CodexProfileAuthV1,
    );
    manager.set(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME, saved)?;
    let ciphertext = std::fs::read(home.path().join("secrets").join("codex_auth.age"))?;
    for mode in [
        AuthCredentialsStoreMode::Keyring,
        AuthCredentialsStoreMode::Auto,
    ] {
        let storage = create_auth_storage_with_store_and_namespace(
            home.path().to_path_buf(),
            mode,
            Arc::clone(&keyring),
            AuthKeyringBackendKind::Secrets,
            AuthStorageNamespace::ProfileV1,
        );
        assert_eq!(storage.load()?, Some(expected.clone()));
        let guard = storage.acquire_lock()?;
        assert_eq!(
            storage.resolved_backend_kind_with_guard(&guard)?,
            Some(PersistentAuthBackendKind::Secrets)
        );
        assert_eq!(
            storage.backend().backend_kind(),
            PersistentAuthBackendKind::Secrets
        );
    }
    assert_eq!(
        std::fs::read(home.path().join("secrets").join("codex_auth.age"))?,
        ciphertext
    );
    Ok(())
}

#[derive(Debug)]
struct RepairFailure;

impl AuthStorageBackend for RepairFailure {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        Ok(None)
    }
    fn save(&self, _auth: &AuthDotJson) -> std::io::Result<()> {
        Ok(())
    }
    fn delete(&self) -> std::io::Result<bool> {
        Ok(false)
    }
    fn repair_durability(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "synthetic durability failure",
        ))
    }
}

#[test]
fn observed_storage_preserves_durability_repair_failures() {
    let observed = storage_telemetry::observe(
        Arc::new(RepairFailure),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    );
    assert_eq!(
        observed
            .repair_durability()
            .expect_err("repair must reach the wrapped backend")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
