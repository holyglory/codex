use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

use super::BedrockAccessKeysAuth;
use super::BedrockApiKeyAuth;
use super::atomic_file::repair_parent_directory_durability;
use super::atomic_file::replace_atomically;
use super::credential_lock::CredentialLock;
use super::credential_lock::CredentialLockGuard;
use crate::token_data::TokenData;
use codex_agent_identity::AgentIdentityJwtClaims;
use codex_agent_identity::decode_agent_identity_jwt;
use codex_config::types::AuthCredentialsStoreMode;
pub use codex_config::types::AuthKeyringBackendKind;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use once_cell::sync::Lazy;

/// Expected structure for $CODEX_HOME/auth.json.
#[derive(Deserialize, Serialize, Clone, PartialEq)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,

    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<AgentIdentityStorage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_access_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_api_key: Option<BedrockApiKeyAuth>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_access_keys: Option<BedrockAccessKeysAuth>,
}

impl Debug for AuthDotJson {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthDotJson")
            .field("auth_mode", &self.auth_mode)
            .field(
                "openai_api_key",
                &self.openai_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("tokens", &self.tokens.as_ref().map(|_| "<redacted>"))
            .field("last_refresh", &self.last_refresh)
            .field(
                "agent_identity",
                &self.agent_identity.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "personal_access_token",
                &self.personal_access_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "bedrock_api_key",
                &self.bedrock_api_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum AgentIdentityStorage {
    Jwt(String),
    Record(AgentIdentityAuthRecord),
}

impl Debug for AgentIdentityStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Jwt(_) => formatter.debug_tuple("Jwt").field(&"<redacted>").finish(),
            Self::Record(_) => formatter
                .debug_tuple("Record")
                .field(&"<redacted>")
                .finish(),
        }
    }
}

impl AgentIdentityStorage {
    pub fn has_auth_material(&self) -> bool {
        match self {
            Self::Jwt(jwt) => !jwt.trim().is_empty(),
            Self::Record(record) => {
                !record.agent_runtime_id.trim().is_empty()
                    && !record.agent_private_key.trim().is_empty()
            }
        }
    }

    pub(crate) fn as_record(&self) -> Option<&AgentIdentityAuthRecord> {
        match self {
            Self::Jwt(_) => None,
            Self::Record(record) => Some(record),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct AgentIdentityAuthRecord {
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_empty_string",
        serialize_with = "serialize_optional_string_as_empty"
    )]
    pub email: Option<String>,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

impl Debug for AgentIdentityAuthRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentIdentityAuthRecord")
            .field("agent_runtime_id", &"<redacted>")
            .field("agent_private_key", &"<redacted>")
            .field("account_id", &"<redacted>")
            .field("chatgpt_user_id", &"<redacted>")
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .field("plan_type", &self.plan_type)
            .field(
                "chatgpt_account_is_fedramp",
                &self.chatgpt_account_is_fedramp,
            )
            .field("task_id", &self.task_id.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|value| value.filter(|value| !value.is_empty()))
}

fn serialize_optional_string_as_empty<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value.as_deref().unwrap_or_default().serialize(serializer)
}

impl AgentIdentityAuthRecord {
    pub(crate) fn from_agent_identity_jwt(jwt: &str) -> std::io::Result<Self> {
        let claims =
            decode_agent_identity_jwt(jwt, /*jwks*/ None).map_err(std::io::Error::other)?;

        Ok(claims.into())
    }
}

impl From<AgentIdentityJwtClaims> for AgentIdentityAuthRecord {
    fn from(claims: AgentIdentityJwtClaims) -> Self {
        Self {
            agent_runtime_id: claims.agent_runtime_id,
            agent_private_key: claims.agent_private_key,
            account_id: claims.account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            email: claims.email,
            plan_type: claims.plan_type.into(),
            chatgpt_account_is_fedramp: claims.chatgpt_account_is_fedramp,
            task_id: None,
        }
    }
}

pub(super) fn get_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

pub(super) fn delete_file_if_exists(codex_home: &Path) -> std::io::Result<bool> {
    let auth_file = get_auth_file(codex_home);
    match std::fs::remove_file(&auth_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn verify_file_absent(codex_home: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(get_auth_file(codex_home)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(std::io::Error::other(
            "credential fallback deletion verification failed",
        )),
        Err(_) => Err(std::io::Error::other(
            "credential fallback deletion could not be verified",
        )),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PersistentAuthBackendKind {
    File,
    DirectKeyring,
    Secrets,
    Ephemeral,
}

pub(super) trait AuthStorageBackend: Debug + Send + Sync {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>>;
    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()>;
    fn delete(&self) -> std::io::Result<bool>;
    fn repair_durability(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn backend_kind(&self) -> PersistentAuthBackendKind;
    fn resolved_backend_kind(&self) -> std::io::Result<Option<PersistentAuthBackendKind>> {
        Ok(self.load()?.map(|_| self.backend_kind()))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AuthStorageNamespace {
    LegacyV0,
    ProfileV1,
}

#[derive(Clone)]
pub(super) struct AuthStorage {
    backend: Arc<dyn AuthStorageBackend>,
    lock: CredentialLock,
    namespace: AuthStorageNamespace,
}

impl Debug for AuthStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthStorage")
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl AuthStorage {
    fn new(
        storage_home: PathBuf,
        backend: Arc<dyn AuthStorageBackend>,
        namespace: AuthStorageNamespace,
    ) -> Self {
        Self {
            backend,
            lock: CredentialLock::new(storage_home),
            namespace,
        }
    }

    pub(super) fn resolved_backend_kind_with_guard(
        &self,
        guard: &CredentialLockGuard,
    ) -> std::io::Result<Option<PersistentAuthBackendKind>> {
        self.lock.verify(guard)?;
        self.backend.resolved_backend_kind()
    }

    pub(super) fn acquire_lock(&self) -> std::io::Result<CredentialLockGuard> {
        self.lock.acquire()
    }

    pub(super) fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.backend.load()
    }

    pub(super) fn load_with_guard(
        &self,
        guard: &CredentialLockGuard,
    ) -> std::io::Result<Option<AuthDotJson>> {
        self.lock.verify(guard)?;
        self.backend.load()
    }

    pub(super) fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let guard = self.acquire_lock()?;
        self.save_with_guard(&guard, auth)
    }

    pub(super) fn save_with_guard(
        &self,
        guard: &CredentialLockGuard,
        auth: &AuthDotJson,
    ) -> std::io::Result<()> {
        self.lock.verify(guard)?;
        self.backend.save(auth)
    }

    pub(super) fn delete(&self) -> std::io::Result<bool> {
        let guard = self.acquire_lock()?;
        self.delete_with_guard(&guard)
    }

    pub(super) fn delete_with_guard(&self, guard: &CredentialLockGuard) -> std::io::Result<bool> {
        self.lock.verify(guard)?;
        self.backend.delete()
    }

    pub(super) fn repair_durability_with_guard(
        &self,
        guard: &CredentialLockGuard,
    ) -> std::io::Result<()> {
        self.lock.verify(guard)?;
        self.backend.repair_durability()
    }
}

#[derive(Clone, Debug)]
pub(super) struct FileAuthStorage {
    codex_home: PathBuf,
}

impl FileAuthStorage {
    pub(super) fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    /// Attempt to read and parse the `auth.json` file in the given `CODEX_HOME` directory.
    /// Returns the full AuthDotJson structure.
    pub(super) fn try_read_auth_json(&self, auth_file: &Path) -> std::io::Result<AuthDotJson> {
        let mut file = File::open(auth_file)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let auth_dot_json: AuthDotJson = serde_json::from_str(&contents)?;

        Ok(auth_dot_json)
    }
}

impl AuthStorageBackend for FileAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let auth_file = get_auth_file(&self.codex_home);
        let auth_dot_json = match self.try_read_auth_json(&auth_file) {
            Ok(auth) => auth,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(auth_dot_json))
    }

    fn save(&self, auth_dot_json: &AuthDotJson) -> std::io::Result<()> {
        let auth_file = get_auth_file(&self.codex_home);

        let json_data = serde_json::to_string_pretty(auth_dot_json)?;
        replace_atomically(&auth_file, json_data.as_bytes())
    }

    fn delete(&self) -> std::io::Result<bool> {
        delete_file_if_exists(&self.codex_home)
    }

    fn repair_durability(&self) -> std::io::Result<()> {
        repair_parent_directory_durability(&get_auth_file(&self.codex_home))
    }

    fn backend_kind(&self) -> PersistentAuthBackendKind {
        PersistentAuthBackendKind::File
    }
}

static CODEX_AUTH_SECRET_NAME: Lazy<SecretName> =
    Lazy::new(|| match SecretName::new("CODEX_AUTH") {
        Ok(name) => name,
        Err(err) => unreachable!("CODEX_AUTH should be a valid secret name: {err}"),
    });
const KEYRING_SERVICE: &str = "Codex Auth";

fn encrypted_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("secrets").join("codex_auth.age")
}

// turns codex_home path into a stable, short key string
fn compute_store_key(codex_home: &Path) -> std::io::Result<String> {
    compute_store_key_with_namespace(codex_home, AuthStorageNamespace::LegacyV0)
}

fn compute_store_key_with_namespace(
    codex_home: &Path,
    namespace: AuthStorageNamespace,
) -> std::io::Result<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = hex.get(..16).unwrap_or(&hex);
    Ok(match namespace {
        AuthStorageNamespace::LegacyV0 => format!("cli|{truncated}"),
        AuthStorageNamespace::ProfileV1 => format!("cli|profile-v1|{truncated}"),
    })
}

#[derive(Clone, Debug)]
struct DirectKeyringAuthStorage {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    namespace: AuthStorageNamespace,
}

impl DirectKeyringAuthStorage {
    #[cfg(test)]
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self::new_with_namespace(codex_home, keyring_store, AuthStorageNamespace::LegacyV0)
    }

    fn new_with_namespace(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        namespace: AuthStorageNamespace,
    ) -> Self {
        Self {
            codex_home,
            keyring_store,
            namespace,
        }
    }

    fn load_from_keyring(&self, key: &str) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_store.load(KEYRING_SERVICE, key) {
            Ok(Some(serialized)) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from keyring: {err}"
                ))
            }),
            Ok(None) => Ok(None),
            Err(_) => Err(std::io::Error::other(
                "failed to load CLI auth from keyring",
            )),
        }
    }

    fn save_to_keyring(&self, key: &str, value: &str) -> std::io::Result<()> {
        match self.keyring_store.save(KEYRING_SERVICE, key, value) {
            Ok(()) => Ok(()),
            Err(_) => {
                let message = "failed to write OAuth tokens to keyring";
                warn!("{message}");
                Err(std::io::Error::other(message))
            }
        }
    }
}

impl AuthStorageBackend for DirectKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let key = compute_store_key_with_namespace(&self.codex_home, self.namespace)?;
        self.load_from_keyring(&key)
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let key = compute_store_key_with_namespace(&self.codex_home, self.namespace)?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.save_to_keyring(&key, &serialized)?;
        delete_file_if_exists(&self.codex_home)?;
        verify_file_absent(&self.codex_home)?;
        self.repair_durability()?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let key = compute_store_key_with_namespace(&self.codex_home, self.namespace)?;
        let keyring_removed = self
            .keyring_store
            .delete(KEYRING_SERVICE, &key)
            .map_err(|_| std::io::Error::other("failed to delete auth from keyring"))?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        verify_file_absent(&self.codex_home)?;
        self.repair_durability()?;
        Ok(keyring_removed || file_removed)
    }

    fn backend_kind(&self) -> PersistentAuthBackendKind {
        PersistentAuthBackendKind::DirectKeyring
    }

    fn repair_durability(&self) -> std::io::Result<()> {
        repair_parent_directory_durability(&get_auth_file(&self.codex_home))
    }
}

#[derive(Clone)]
struct SecretsKeyringAuthStorage {
    codex_home: PathBuf,
    direct_storage: DirectKeyringAuthStorage,
    secrets_manager: SecretsManager,
}

impl Debug for SecretsKeyringAuthStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsKeyringAuthStorage")
            .finish_non_exhaustive()
    }
}

impl SecretsKeyringAuthStorage {
    #[cfg(test)]
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self::new_with_namespace(codex_home, keyring_store, AuthStorageNamespace::LegacyV0)
    }

    fn new_with_namespace(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        namespace: AuthStorageNamespace,
    ) -> Self {
        let direct_storage = DirectKeyringAuthStorage::new_with_namespace(
            codex_home.clone(),
            Arc::clone(&keyring_store),
            namespace,
        );
        let secrets_namespace = match namespace {
            AuthStorageNamespace::LegacyV0 => LocalSecretsNamespace::CodexAuth,
            AuthStorageNamespace::ProfileV1 => LocalSecretsNamespace::CodexProfileAuthV1,
        };
        let secrets_manager = SecretsManager::new_with_keyring_store_and_namespace(
            codex_home.clone(),
            SecretsBackendKind::Local,
            keyring_store,
            secrets_namespace,
        );
        Self {
            codex_home,
            direct_storage,
            secrets_manager,
        }
    }
}

impl AuthStorageBackend for SecretsKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self
            .secrets_manager
            .get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|_| std::io::Error::other("failed to load encrypted CLI auth"))?
        {
            Some(serialized) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from encrypted auth storage: {err}"
                ))
            }),
            None => Ok(None),
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.secrets_manager
            .set(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME, &serialized)
            .map_err(|_| {
                let message = "failed to write OAuth tokens to encrypted auth storage";
                warn!("{message}");
                std::io::Error::other(message)
            })?;
        delete_file_if_exists(&self.codex_home)?;
        verify_file_absent(&self.codex_home)?;
        self.repair_durability()?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let keyring_removed = self
            .secrets_manager
            .delete(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|_| std::io::Error::other("failed to delete encrypted CLI auth"))?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        let direct_removed = self.direct_storage.delete()?;
        if self
            .secrets_manager
            .get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|_| std::io::Error::other("failed to verify encrypted CLI auth deletion"))?
            .is_some()
            || self.direct_storage.load()?.is_some()
        {
            return Err(std::io::Error::other(
                "credential deletion verification failed",
            ));
        }
        verify_file_absent(&self.codex_home)?;
        self.repair_durability()?;
        Ok(keyring_removed || file_removed || direct_removed)
    }

    fn backend_kind(&self) -> PersistentAuthBackendKind {
        PersistentAuthBackendKind::Secrets
    }

    fn repair_durability(&self) -> std::io::Result<()> {
        repair_parent_directory_durability(&get_auth_file(&self.codex_home))?;
        repair_parent_directory_durability(&encrypted_auth_file(&self.codex_home))
    }
}

#[derive(Clone, Debug)]
struct AutoAuthStorage {
    keyring_storage: Arc<dyn AuthStorageBackend>,
    file_storage: Arc<FileAuthStorage>,
}

impl AutoAuthStorage {
    #[cfg(test)]
    fn new(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self::new_with_namespace(
            codex_home,
            keyring_store,
            keyring_backend_kind,
            AuthStorageNamespace::LegacyV0,
        )
    }

    fn new_with_namespace(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
        namespace: AuthStorageNamespace,
    ) -> Self {
        Self {
            keyring_storage: create_keyring_auth_storage(
                codex_home.clone(),
                keyring_store,
                keyring_backend_kind,
                namespace,
            ),
            file_storage: Arc::new(FileAuthStorage::new(codex_home)),
        }
    }
}

impl AuthStorageBackend for AutoAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_storage.load() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.file_storage.load(),
            Err(err) => {
                warn!("failed to load CLI auth from keyring, falling back to file storage: {err}");
                self.file_storage.load()
            }
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        match self.keyring_storage.save(auth) {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!("failed to save auth to keyring, falling back to file storage: {err}");
                self.file_storage.save(auth)
            }
        }
    }

    fn delete(&self) -> std::io::Result<bool> {
        // Keyring storage will delete from disk as well
        self.keyring_storage.delete()
    }

    fn backend_kind(&self) -> PersistentAuthBackendKind {
        self.keyring_storage.backend_kind()
    }

    fn resolved_backend_kind(&self) -> std::io::Result<Option<PersistentAuthBackendKind>> {
        match self.keyring_storage.load() {
            Ok(Some(_)) => Ok(Some(self.keyring_storage.backend_kind())),
            Ok(None) | Err(_) => Ok(self
                .file_storage
                .load()?
                .map(|_| PersistentAuthBackendKind::File)),
        }
    }

    fn repair_durability(&self) -> std::io::Result<()> {
        self.keyring_storage.repair_durability()?;
        self.file_storage.repair_durability()
    }
}

// A global in-memory store for mapping codex_home -> AuthDotJson.
static EPHEMERAL_AUTH_STORE: Lazy<Mutex<HashMap<String, AuthDotJson>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug)]
struct EphemeralAuthStorage {
    codex_home: PathBuf,
}

impl EphemeralAuthStorage {
    fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn with_store<F, T>(&self, action: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut HashMap<String, AuthDotJson>, String) -> std::io::Result<T>,
    {
        let key = compute_store_key(&self.codex_home)?;
        let mut store = EPHEMERAL_AUTH_STORE
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock ephemeral auth storage"))?;
        action(&mut store, key)
    }
}

impl AuthStorageBackend for EphemeralAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, auth.clone());
            Ok(())
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.with_store(|store, key| Ok(store.remove(&key).is_some()))
    }

    fn backend_kind(&self) -> PersistentAuthBackendKind {
        PersistentAuthBackendKind::Ephemeral
    }
}

pub(super) fn create_auth_storage(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<AuthStorage> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    create_auth_storage_with_store(codex_home, mode, keyring_store, keyring_backend_kind)
}

pub(super) fn create_auth_storage_with_namespace(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    namespace: AuthStorageNamespace,
) -> Arc<AuthStorage> {
    create_auth_storage_with_store_and_namespace(
        codex_home,
        mode,
        Arc::new(DefaultKeyringStore),
        keyring_backend_kind,
        namespace,
    )
}

pub(super) fn create_auth_storage_with_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<AuthStorage> {
    create_auth_storage_with_store_and_namespace(
        codex_home,
        mode,
        keyring_store,
        keyring_backend_kind,
        AuthStorageNamespace::LegacyV0,
    )
}

pub(super) fn create_auth_storage_with_store_and_namespace(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
    namespace: AuthStorageNamespace,
) -> Arc<AuthStorage> {
    let backend: Arc<dyn AuthStorageBackend> = match mode {
        AuthCredentialsStoreMode::File => Arc::new(FileAuthStorage::new(codex_home.clone())),
        AuthCredentialsStoreMode::Keyring => create_keyring_auth_storage(
            codex_home.clone(),
            keyring_store,
            keyring_backend_kind,
            namespace,
        ),
        AuthCredentialsStoreMode::Auto => Arc::new(AutoAuthStorage::new_with_namespace(
            codex_home.clone(),
            keyring_store,
            keyring_backend_kind,
            namespace,
        )),
        AuthCredentialsStoreMode::Ephemeral => {
            Arc::new(EphemeralAuthStorage::new(codex_home.clone()))
        }
    };
    Arc::new(AuthStorage::new(codex_home, backend, namespace))
}

pub(super) fn create_auth_storage_for_backend(
    codex_home: PathBuf,
    backend_kind: PersistentAuthBackendKind,
    keyring_store: Arc<dyn KeyringStore>,
    namespace: AuthStorageNamespace,
) -> Arc<AuthStorage> {
    let backend: Arc<dyn AuthStorageBackend> = match backend_kind {
        PersistentAuthBackendKind::File => Arc::new(FileAuthStorage::new(codex_home.clone())),
        PersistentAuthBackendKind::DirectKeyring => {
            Arc::new(DirectKeyringAuthStorage::new_with_namespace(
                codex_home.clone(),
                keyring_store,
                namespace,
            ))
        }
        PersistentAuthBackendKind::Secrets => {
            Arc::new(SecretsKeyringAuthStorage::new_with_namespace(
                codex_home.clone(),
                keyring_store,
                namespace,
            ))
        }
        PersistentAuthBackendKind::Ephemeral => {
            Arc::new(EphemeralAuthStorage::new(codex_home.clone()))
        }
    };
    Arc::new(AuthStorage::new(codex_home, backend, namespace))
}

fn create_keyring_auth_storage(
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
    namespace: AuthStorageNamespace,
) -> Arc<dyn AuthStorageBackend> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => Arc::new(DirectKeyringAuthStorage::new_with_namespace(
            codex_home,
            keyring_store,
            namespace,
        )),
        AuthKeyringBackendKind::Secrets => Arc::new(SecretsKeyringAuthStorage::new_with_namespace(
            codex_home,
            keyring_store,
            namespace,
        )),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
