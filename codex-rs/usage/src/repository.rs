use crate::facts::FactEventId;
use crate::store::UsageStore;
use crate::store::UsageStoreError;
use codex_private_storage::AtomicWriteMode;
use codex_private_storage::PrivateStorageError;
use codex_private_storage::ensure_private_file;
use codex_private_storage::write_file_atomically;
use hmac::Hmac;
use hmac::Mac;
use rand::RngCore;
use sha2::Sha256;
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

const REPOSITORY_KEY_FILENAME: &str = "repository-hmac.key";
const REPOSITORY_KEY_BYTES: usize = 32;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_LABEL_BYTES: usize = 80;
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RepositoryIdentityError {
    #[error("repository identity material is invalid")]
    InvalidIdentity,
    #[error("repository display label is invalid")]
    InvalidLabel,
    #[error("repository location must be a caller-canonicalized absolute path")]
    InvalidCanonicalPath,
}

/// Raw repository identity accepted only at the hashing boundary.
///
/// Deliberately does not implement `Debug`, `Display`, serialization, or cloning.
pub struct RepositoryIdentityMaterial(String);

impl RepositoryIdentityMaterial {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryIdentityError> {
        let value = value.into();
        if value.trim().is_empty()
            || value.len() > MAX_IDENTITY_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(RepositoryIdentityError::InvalidIdentity);
        }
        Ok(Self(value))
    }
}

/// Absolute canonical path supplied by the caller after symlink resolution.
///
/// This boundary validates shape only; callers must use their platform's
/// canonicalization API before constructing it.
pub struct CanonicalRepositoryPath(String);

impl CanonicalRepositoryPath {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryIdentityError> {
        let value = value.into();
        let normalized = normalize_canonical_path(&value);
        (value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control))
            .then_some(normalized)
            .flatten()
            .map(Self)
            .ok_or(RepositoryIdentityError::InvalidCanonicalPath)
    }
}

/// Resolves a checkout's Git common directory without invoking Git or persisting the path.
pub fn discover_git_common_dir(workspace: &Path) -> Option<CanonicalRepositoryPath> {
    let git_entry = workspace.join(".git");
    if git_entry.is_dir() {
        let canonical = std::fs::canonicalize(git_entry).ok()?;
        return CanonicalRepositoryPath::new(canonical.to_str()?).ok();
    }
    let pointer = std::fs::read_to_string(&git_entry).ok()?;
    if pointer.len() > 4_096 || pointer.lines().count() != 1 {
        return None;
    }
    let git_dir = pointer.strip_prefix("gitdir:")?.trim();
    let git_dir = if Path::new(git_dir).is_absolute() {
        Path::new(git_dir).to_path_buf()
    } else {
        workspace.join(git_dir)
    };
    let git_dir = std::fs::canonicalize(git_dir).ok()?;
    let common_pointer = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(pointer) => pointer,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CanonicalRepositoryPath::new(git_dir.to_str()?).ok();
        }
        Err(_) => return None,
    };
    if common_pointer.len() > 4_096 || common_pointer.lines().count() != 1 {
        return None;
    }
    let common = common_pointer.trim();
    let common = if Path::new(common).is_absolute() {
        Path::new(common).to_path_buf()
    } else {
        git_dir.join(common)
    };
    let common = std::fs::canonicalize(common).ok()?;
    CanonicalRepositoryPath::new(common.to_str()?).ok()
}

fn normalize_canonical_path(value: &str) -> Option<String> {
    if value == "/" {
        return Some(value.to_string());
    }
    if value.starts_with('/') && !value.starts_with("//") && !value.contains('\\') {
        valid_path_components(value.split('/').skip(1), /*allow_trailing_root*/ false)
            .then(|| value.to_string())
    } else if value.len() >= 3
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
        && matches!(value.as_bytes()[2], b'/' | b'\\')
    {
        let mut normalized = value.replace('\\', "/");
        normalized.replace_range(0..1, &normalized[0..1].to_ascii_lowercase());
        let components = normalized[3..].split('/');
        if normalized.len() == 3
            || valid_path_components(components, /*allow_trailing_root*/ false)
        {
            Some(normalized)
        } else {
            None
        }
    } else if value.starts_with("\\\\") || value.starts_with("//") {
        let normalized = value.replace('\\', "/");
        let components = normalized[2..].split('/').collect::<Vec<_>>();
        (components.len() >= 2
            && components
                .iter()
                .all(|component| !matches!(*component, "" | "." | "..")))
        .then_some(normalized)
    } else {
        None
    }
}

fn valid_path_components<'a>(
    components: impl Iterator<Item = &'a str>,
    allow_trailing_root: bool,
) -> bool {
    let components = components.collect::<Vec<_>>();
    !components.is_empty()
        && components.iter().enumerate().all(|(index, component)| {
            (!component.is_empty()
                || (allow_trailing_root && index == components.len().saturating_sub(1)))
                && !matches!(*component, "." | "..")
        })
}

/// Caller-supplied identity candidates in descending authority order.
pub struct RepositoryIdentityInput {
    origin: Option<RepositoryIdentityMaterial>,
    git_common_dir: Option<CanonicalRepositoryPath>,
    workspace: CanonicalRepositoryPath,
}

impl RepositoryIdentityInput {
    pub fn new(workspace: CanonicalRepositoryPath) -> Self {
        Self {
            origin: None,
            git_common_dir: None,
            workspace,
        }
    }

    pub fn with_origin(mut self, origin: RepositoryIdentityMaterial) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn with_git_common_dir(mut self, common_dir: CanonicalRepositoryPath) -> Self {
        self.git_common_dir = Some(common_dir);
        self
    }

    fn selected(&self) -> (RepositoryIdentitySource, String) {
        if let Some(origin) = &self.origin {
            return (
                RepositoryIdentitySource::Origin,
                normalize_origin(&origin.0),
            );
        }
        if let Some(common_dir) = &self.git_common_dir {
            return (RepositoryIdentitySource::GitCommonDir, common_dir.0.clone());
        }
        (
            RepositoryIdentitySource::Workspace,
            self.workspace.0.clone(),
        )
    }

    fn candidates(&self) -> Vec<(RepositoryIdentitySource, String)> {
        let mut candidates = Vec::with_capacity(3);
        if let Some(origin) = &self.origin {
            candidates.push((
                RepositoryIdentitySource::Origin,
                normalize_origin(&origin.0),
            ));
        }
        if let Some(common_dir) = &self.git_common_dir {
            candidates.push((RepositoryIdentitySource::GitCommonDir, common_dir.0.clone()));
        }
        candidates.push((
            RepositoryIdentitySource::Workspace,
            self.workspace.0.clone(),
        ));
        candidates
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryIdentitySource {
    Origin,
    GitCommonDir,
    Workspace,
}

impl RepositoryIdentitySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::GitCommonDir => "git_common_dir",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepositoryId(String);

impl RepositoryId {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryIdentityError> {
        let value = value.into();
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        valid
            .then_some(Self(value))
            .ok_or(RepositoryIdentityError::InvalidIdentity)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeRepositoryLabel(String);

impl SafeRepositoryLabel {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryIdentityError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_LABEL_BYTES
            || matches!(trimmed, "." | "..")
            || !trimmed
                .chars()
                .all(|character| character.is_alphanumeric() || " -_.".contains(character))
        {
            return Err(RepositoryIdentityError::InvalidLabel);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) struct RepositoryHmacKey([u8; REPOSITORY_KEY_BYTES]);

impl Drop for RepositoryHmacKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl RepositoryHmacKey {
    fn repository_id(
        &self,
        source: RepositoryIdentitySource,
        normalized_identity: &str,
    ) -> Result<RepositoryId, UsageStoreError> {
        let mut mac = HmacSha256::new_from_slice(&self.0)
            .map_err(|_| UsageStoreError::RepositoryKeyCorrupt)?;
        mac.update(b"codex-repository-identity-v1\0");
        mac.update(source.as_str().as_bytes());
        mac.update(b"\0");
        mac.update(normalized_identity.as_bytes());
        Ok(RepositoryId(hex_lower(&mac.finalize().into_bytes())))
    }
}

pub(crate) async fn load_or_create_repository_key(
    usage_dir: &Path,
    pool: &SqlitePool,
) -> Result<RepositoryHmacKey, UsageStoreError> {
    let path = usage_dir.join(REPOSITORY_KEY_FILENAME);
    match std::fs::read(&path) {
        Ok(bytes) => {
            ensure_private_file(&path).map_err(private_storage_filesystem)?;
            return repository_key_from_bytes(bytes);
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(UsageStoreError::Filesystem(error));
        }
        Err(_) => {}
    }
    let has_history: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories LIMIT 1)")
        .fetch_one(pool)
        .await
        .map_err(UsageStoreError::Database)?;
    if has_history != 0 {
        return Err(UsageStoreError::RepositoryKeyMissing);
    }

    let mut bytes = [0_u8; REPOSITORY_KEY_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    install_key_atomically(&path, &bytes)?;
    ensure_private_file(&path).map_err(private_storage_filesystem)?;
    repository_key_from_bytes(std::fs::read(path).map_err(UsageStoreError::Filesystem)?)
}

impl UsageStore {
    pub fn repository_id_for_identity(
        &self,
        identity: &RepositoryIdentityInput,
    ) -> Result<RepositoryId, UsageStoreError> {
        let (source, normalized) = identity.selected();
        self.repository_key.repository_id(source, &normalized)
    }

    /// Returns privacy-preserving candidate IDs in descending identity authority.
    ///
    /// This lets readers match history captured when less repository metadata was
    /// available without exposing or persisting the raw identity material.
    pub fn repository_ids_for_identity_candidates(
        &self,
        identity: &RepositoryIdentityInput,
    ) -> Result<Vec<RepositoryId>, UsageStoreError> {
        let mut ids = Vec::with_capacity(3);
        for (source, normalized) in identity.candidates() {
            let id = self.repository_key.repository_id(source, &normalized)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Finds the highest-authority stored identity available for one checkout.
    ///
    /// Lookup is deliberately read-only. Simultaneously observing multiple
    /// identity sources does not prove that their historical records should be
    /// merged after a remote change or workspace-path reuse.
    pub async fn find_repository_for_identity(
        &self,
        identity: &RepositoryIdentityInput,
    ) -> Result<Option<RepositoryId>, UsageStoreError> {
        let candidate_ids = self.repository_ids_for_identity_candidates(identity)?;
        for candidate in candidate_ids {
            if self.repository_exists(&candidate).await? {
                return self.canonical_repository_id(&candidate).await.map(Some);
            }
        }
        Ok(None)
    }

    pub async fn resolve_repository(
        &self,
        identity: &RepositoryIdentityInput,
        label: &SafeRepositoryLabel,
        observed_at_ms: i64,
    ) -> Result<RepositoryId, UsageStoreError> {
        let (source, normalized) = identity.selected();
        let repository_id = self.repository_key.repository_id(source, &normalized)?;
        let mut transaction = self.pool.begin().await.map_err(UsageStoreError::Database)?;
        sqlx::query(
            r#"
            INSERT INTO repositories(id, identity_source, safe_display_label, created_at_ms)
            VALUES (?, ?, ?, ?) ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(repository_id.as_str())
        .bind(source.as_str())
        .bind(&label.0)
        .bind(observed_at_ms)
        .execute(&mut *transaction)
        .await
        .map_err(UsageStoreError::Database)?;
        sqlx::query(
            r#"
            INSERT INTO repository_seen_events(event_id, repository_id, occurred_at_ms)
            VALUES (?, ?, ?) ON CONFLICT(repository_id, occurred_at_ms) DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(repository_id.as_str())
        .bind(observed_at_ms)
        .execute(&mut *transaction)
        .await
        .map_err(UsageStoreError::Database)?;
        transaction
            .commit()
            .await
            .map_err(UsageStoreError::Database)?;
        Ok(repository_id)
    }

    pub async fn append_repository_alias(
        &self,
        event_id: FactEventId,
        repository_id: &RepositoryId,
        alias: &SafeRepositoryLabel,
        occurred_at_ms: i64,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO repository_alias_events(event_id, repository_id, safe_alias, occurred_at_ms)
            VALUES (?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING
            "#,
        )
        .bind(event_id.as_string())
        .bind(repository_id.as_str())
        .bind(&alias.0)
        .bind(occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !self
                .repository_alias_matches(event_id, repository_id, alias, occurred_at_ms)
                .await?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn append_repository_merge(
        &self,
        event_id: FactEventId,
        source: &RepositoryId,
        target: &RepositoryId,
        occurred_at_ms: i64,
    ) -> Result<(), UsageStoreError> {
        let mut transaction = self.pool.begin().await.map_err(UsageStoreError::Database)?;
        let creates_cycle: i64 = sqlx::query_scalar(
            r#"
            WITH RECURSIVE target_chain(id) AS (
                SELECT ?
                UNION
                SELECT merge.target_repository_id
                FROM repository_merge_events AS merge
                JOIN target_chain ON merge.source_repository_id = target_chain.id
            )
            SELECT EXISTS(SELECT 1 FROM target_chain WHERE id = ?)
            "#,
        )
        .bind(target.as_str())
        .bind(source.as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(UsageStoreError::Database)?;
        if creates_cycle != 0 {
            return Err(UsageStoreError::RepositoryMergeCycle);
        }
        let result = sqlx::query(
            r#"
            INSERT INTO repository_merge_events(
                event_id, source_repository_id, target_repository_id, occurred_at_ms
            ) VALUES (?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING
            "#,
        )
        .bind(event_id.as_string())
        .bind(source.as_str())
        .bind(target.as_str())
        .bind(occurred_at_ms)
        .execute(&mut *transaction)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !repository_merge_matches(&mut transaction, event_id, source, target, occurred_at_ms)
                .await?
        {
            return Err(UsageStoreError::FactConflict);
        }
        transaction
            .commit()
            .await
            .map_err(UsageStoreError::Database)?;
        Ok(())
    }

    pub async fn canonical_repository_id(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<RepositoryId, UsageStoreError> {
        let mut current = repository_id.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.0.clone()) {
            let target = sqlx::query_scalar::<_, String>(
                r#"
                SELECT target_repository_id FROM repository_merge_events
                WHERE source_repository_id = ?
                "#,
            )
            .bind(current.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
            let Some(target) = target else {
                return Ok(current);
            };
            current = RepositoryId(target);
        }
        Err(UsageStoreError::RepositoryMergeCycle)
    }

    pub async fn repository_display_label(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<String, UsageStoreError> {
        let canonical = self.canonical_repository_id(repository_id).await?;
        if let Some(alias) = sqlx::query_scalar::<_, String>(
            r#"
            WITH RECURSIVE family(id) AS (
                SELECT ?
                UNION
                SELECT merge.source_repository_id
                FROM repository_merge_events AS merge
                JOIN family ON merge.target_repository_id = family.id
            )
            SELECT alias.safe_alias
            FROM repository_alias_events AS alias
            WHERE alias.repository_id IN (SELECT id FROM family)
            ORDER BY alias.occurred_at_ms DESC, alias.event_id DESC
            LIMIT 1
            "#,
        )
        .bind(canonical.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?
        {
            return Ok(alias);
        }
        sqlx::query_scalar("SELECT safe_display_label FROM repositories WHERE id = ?")
            .bind(canonical.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)
    }

    pub(crate) async fn repository_family_ids(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<HashSet<String>, UsageStoreError> {
        let canonical = self.canonical_repository_id(repository_id).await?;
        let ids = sqlx::query_scalar::<_, String>(
            r#"
            WITH RECURSIVE family(id) AS (
                SELECT ?
                UNION
                SELECT merge.source_repository_id
                FROM repository_merge_events AS merge
                JOIN family ON merge.target_repository_id = family.id
            )
            SELECT id FROM family
            "#,
        )
        .bind(canonical.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(ids.into_iter().collect())
    }

    async fn repository_alias_matches(
        &self,
        event_id: FactEventId,
        repository_id: &RepositoryId,
        alias: &SafeRepositoryLabel,
        occurred_at_ms: i64,
    ) -> Result<bool, UsageStoreError> {
        let stored = sqlx::query_as::<_, (String, String, i64)>(
            r#"
            SELECT repository_id, safe_alias, occurred_at_ms
            FROM repository_alias_events WHERE event_id = ?
            "#,
        )
        .bind(event_id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(stored.is_some_and(|stored| {
            stored
                == (
                    repository_id.as_str().to_string(),
                    alias.0.clone(),
                    occurred_at_ms,
                )
        }))
    }
}

async fn repository_merge_matches(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event_id: FactEventId,
    source: &RepositoryId,
    target: &RepositoryId,
    occurred_at_ms: i64,
) -> Result<bool, UsageStoreError> {
    let stored = sqlx::query_as::<_, (String, String, i64)>(
        r#"
        SELECT source_repository_id, target_repository_id, occurred_at_ms
        FROM repository_merge_events WHERE event_id = ?
        "#,
    )
    .bind(event_id.as_string())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(UsageStoreError::Database)?;
    Ok(stored.is_some_and(|stored| {
        stored
            == (
                source.as_str().to_string(),
                target.as_str().to_string(),
                occurred_at_ms,
            )
    }))
}

fn repository_key_from_bytes(bytes: Vec<u8>) -> Result<RepositoryHmacKey, UsageStoreError> {
    let bytes: [u8; REPOSITORY_KEY_BYTES] = bytes
        .try_into()
        .map_err(|_| UsageStoreError::RepositoryKeyCorrupt)?;
    Ok(RepositoryHmacKey(bytes))
}

fn install_key_atomically(final_path: &Path, bytes: &[u8]) -> Result<(), UsageStoreError> {
    match write_file_atomically(final_path, bytes, AtomicWriteMode::NoClobber) {
        Ok(()) | Err(PrivateStorageError::AlreadyExists) => Ok(()),
        Err(PrivateStorageError::CommittedCleanupUncertain { source }) => Err(
            UsageStoreError::RepositoryKeyCommittedCleanupUncertain(source),
        ),
        Err(PrivateStorageError::CommittedDurabilityUncertain { source }) => {
            Err(UsageStoreError::RepositoryKeyCommittedSyncUncertain(source))
        }
        Err(PrivateStorageError::CommittedProtectionUncertain) => {
            Err(UsageStoreError::RepositoryKeyCommittedSyncUncertain(
                std::io::Error::other("repository key protection is uncertain"),
            ))
        }
        Err(error) => Err(private_storage_filesystem(error)),
    }
}

fn private_storage_filesystem(error: PrivateStorageError) -> UsageStoreError {
    UsageStoreError::Filesystem(std::io::Error::other(error))
}

fn normalize_origin(value: &str) -> String {
    let value = value.trim().replace('\\', "/");
    if let Some((scheme, remainder)) = value.split_once("://") {
        let (authority, path) = remainder.split_once('/').unwrap_or((remainder, ""));
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        return format!(
            "{}://{}/{}",
            scheme.to_ascii_lowercase(),
            authority.to_ascii_lowercase(),
            trim_repository_suffix(path)
        );
    }
    if let Some((authority, path)) = value.split_once(':')
        && !authority.contains('/')
    {
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        return format!(
            "{}:{}",
            authority.to_ascii_lowercase(),
            trim_repository_suffix(path)
        );
    }
    trim_repository_suffix(&value)
}

fn trim_repository_suffix(value: &str) -> String {
    value
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(value.trim_matches('/'))
        .to_string()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
#[path = "repository_tests.rs"]
mod tests;
