use super::*;

impl LocalSecretsBackend {
    /// Retires an empty, independently keyed profile-auth store after credential deletion.
    /// Callers must hold the profile's exclusive use, credential, and backend write locks.
    /// Shared namespaces, unknown documents, and nonempty stores are never retired.
    pub fn retire_empty_profile_auth_store(&self) -> Result<()> {
        anyhow::ensure!(
            matches!(self.namespace, LocalSecretsNamespace::CodexProfileAuthV1),
            "only an independently keyed profile auth store can be retired"
        );
        let path = self.secrets_path();
        let account = compute_keyring_account(&self.codex_home, self.namespace);
        let key = self
            .keyring_store
            .load(keyring_service(), &account)
            .map_err(std::io::Error::from)
            .context("failed to load profile secrets key")?
            .map(SecretString::from);
        let mut cache = self
            .decrypted_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match fs::symlink_metadata(self.secrets_dir()) {
            Ok(metadata) => anyhow::ensure!(
                metadata.file_type().is_dir(),
                "profile secrets directory is not independently owned"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_file(),
                    "profile secrets document is not independently owned"
                );
                let key = key.as_ref().context("profile secrets key is unknown")?;
                let plaintext = decrypt_with_passphrase(&fs::read(&path)?, key)?;
                let document: serde_json::Value = serde_json::from_slice(&plaintext)
                    .context("profile secrets document is unsupported")?;
                anyhow::ensure!(
                    document.as_object().is_some_and(|object| object.len() == 2)
                        && document["version"] == SECRETS_VERSION
                        && document["secrets"]
                            .as_object()
                            .is_some_and(serde_json::Map::is_empty),
                    "profile secrets document is unknown or nonempty"
                );
                fs::remove_file(&path)
                    .context("failed to retire empty profile secrets document")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if cache.as_ref().is_some_and(|cached| cached.path == path) {
            *cache = None;
        }
        if key.is_some() {
            self.keyring_store
                .delete(keyring_service(), &account)
                .map_err(std::io::Error::from)
                .context("failed to retire profile secrets key")?;
            anyhow::ensure!(
                self.keyring_store
                    .load(keyring_service(), &account)
                    .map_err(std::io::Error::from)?
                    .is_none(),
                "profile secrets key remains after retirement"
            );
        }
        Ok(())
    }
}
