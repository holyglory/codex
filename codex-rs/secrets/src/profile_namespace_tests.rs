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
