//! Load an independently encrypted historical profile file and its exact saved key name.

use super::*;
use codex_keyring_store::tests::MockKeyringStore;
use pretty_assertions::assert_eq;

#[test]
fn historical_profile_auth_file_uses_its_saved_key_without_legacy_fallback() -> Result<()> {
    let _cache_lock = SECRETS_CACHE_TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let home = tempfile::tempdir()?;
    let canonical = home.path().canonicalize()?;
    let hash = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let suffix = format!("{hash:x}")[..16].to_string();
    let profile_key = format!("secrets|codex-profile-auth-v1|{suffix}");
    let legacy_key = format!("secrets|{suffix}");
    let keyring = Arc::new(MockKeyringStore::default());
    let passphrase = "synthetic historical profile passphrase";
    keyring.save("codex", &profile_key, passphrase)?;
    keyring.save(
        "codex",
        &legacy_key,
        "synthetic unrelated legacy passphrase",
    )?;
    let plaintext =
        br#"{"version":1,"secrets":{"global/CODEX_AUTH":"synthetic historical profile data"}}"#;
    let ciphertext =
        encrypt_with_passphrase(plaintext, &SecretString::from(passphrase.to_string()))?;
    let path = home.path().join("secrets").join("codex_auth.age");
    std::fs::create_dir_all(path.parent().expect("secrets directory"))?;
    std::fs::write(&path, &ciphertext)?;
    let profile = LocalSecretsBackend::new_with_namespace(
        home.path().to_path_buf(),
        keyring.clone(),
        LocalSecretsNamespace::CodexProfileAuthV1,
    );
    let legacy = LocalSecretsBackend::new_with_namespace(
        home.path().to_path_buf(),
        keyring.clone(),
        LocalSecretsNamespace::CodexAuth,
    );
    let name = SecretName::new("CODEX_AUTH")?;
    assert_eq!(
        profile.get(&SecretScope::Global, &name)?,
        Some("synthetic historical profile data".to_string())
    );
    assert!(legacy.get(&SecretScope::Global, &name).is_err());
    assert!(profile.retire_empty_profile_auth_store().is_err());
    assert!(legacy.retire_empty_profile_auth_store().is_err());
    assert_eq!(std::fs::read(path)?, ciphertext);
    assert_eq!(
        keyring.saved_value(&profile_key),
        Some(passphrase.to_string())
    );
    assert_eq!(
        keyring.saved_value(&legacy_key),
        Some("synthetic unrelated legacy passphrase".to_string())
    );
    Ok(())
}

#[test]
fn empty_profile_retirement_preserves_shared_namespace_keys() -> Result<()> {
    let _cache_lock = SECRETS_CACHE_TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for namespace in [
        LocalSecretsNamespace::CodexProfileAuthV1,
        LocalSecretsNamespace::CodexAuth,
    ] {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let profile_key =
            compute_keyring_account(home.path(), LocalSecretsNamespace::CodexProfileAuthV1);
        let shared_key = compute_keyring_account(home.path(), LocalSecretsNamespace::CodexAuth);
        keyring.save(keyring_service(), &shared_key, "synthetic shared key")?;
        let backend = LocalSecretsBackend::new_with_namespace(
            home.path().to_path_buf(),
            keyring.clone(),
            namespace,
        );
        backend.save_file(&SecretsFile::new_empty())?;
        let path = backend.secrets_path();
        let ciphertext = fs::read(&path)?;
        if matches!(namespace, LocalSecretsNamespace::CodexProfileAuthV1) {
            backend.retire_empty_profile_auth_store()?;
            backend.retire_empty_profile_auth_store()?;
            assert!(!path.exists());
            assert_eq!(keyring.saved_value(&profile_key), None);
        } else {
            assert!(backend.retire_empty_profile_auth_store().is_err());
            assert_eq!(fs::read(&path)?, ciphertext);
        }
        assert_eq!(
            keyring.saved_value(&shared_key),
            Some("synthetic shared key".to_string())
        );
    }
    Ok(())
}

#[test]
fn profile_retirement_does_not_generate_or_fall_back_to_a_shared_key() -> Result<()> {
    let _cache_lock = SECRETS_CACHE_TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let home = tempfile::tempdir()?;
    let keyring = Arc::new(MockKeyringStore::default());
    let profile_key =
        compute_keyring_account(home.path(), LocalSecretsNamespace::CodexProfileAuthV1);
    let shared_key = compute_keyring_account(home.path(), LocalSecretsNamespace::CodexAuth);
    let legacy = LocalSecretsBackend::new_with_namespace(
        home.path().to_path_buf(),
        keyring.clone(),
        LocalSecretsNamespace::CodexAuth,
    );
    legacy.save_file(&SecretsFile::new_empty())?;
    let saved_shared_key = keyring.saved_value(&shared_key);
    let path = legacy.secrets_path();
    let ciphertext = fs::read(&path)?;
    let profile = LocalSecretsBackend::new_with_namespace(
        home.path().to_path_buf(),
        keyring.clone(),
        LocalSecretsNamespace::CodexProfileAuthV1,
    );

    assert!(profile.retire_empty_profile_auth_store().is_err());
    assert_eq!(fs::read(path)?, ciphertext);
    assert_eq!(keyring.saved_value(&profile_key), None);
    assert_eq!(keyring.saved_value(&shared_key), saved_shared_key);
    Ok(())
}

#[test]
fn profile_retirement_preserves_unknown_or_nonempty_documents() -> Result<()> {
    let _cache_lock = SECRETS_CACHE_TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for document in [
        serde_json::json!({"version": 1, "secrets": {}, "unknown": "retained"}),
        serde_json::json!({"version": 1, "secrets": {"unknown/key": "retained"}}),
    ] {
        let home = tempfile::tempdir()?;
        let keyring = Arc::new(MockKeyringStore::default());
        let profile = LocalSecretsBackend::new_with_namespace(
            home.path().to_path_buf(),
            keyring.clone(),
            LocalSecretsNamespace::CodexProfileAuthV1,
        );
        let account =
            compute_keyring_account(home.path(), LocalSecretsNamespace::CodexProfileAuthV1);
        let passphrase = "synthetic profile key";
        keyring.save(keyring_service(), &account, passphrase)?;
        let ciphertext = encrypt_with_passphrase(
            &serde_json::to_vec(&document)?,
            &SecretString::from(passphrase.to_string()),
        )?;
        let path = profile.secrets_path();
        fs::create_dir_all(profile.secrets_dir())?;
        fs::write(&path, &ciphertext)?;

        assert!(profile.retire_empty_profile_auth_store().is_err());
        assert_eq!(fs::read(path)?, ciphertext);
        assert_eq!(keyring.saved_value(&account), Some(passphrase.to_string()));
    }
    Ok(())
}
