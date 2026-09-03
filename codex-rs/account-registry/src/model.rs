use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::PlanType;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;
use thiserror::Error;
use uuid::Uuid;

pub const REGISTRY_VERSION: u32 = 1;
/// Priority assigned to newly created and migrated account profiles.
pub const DEFAULT_ACCOUNT_PRIORITY: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentifierError {
    #[error("account id must use the canonical `acct_` UUID form")]
    InvalidAccountId,
    #[error(
        "alias must be 1-64 lowercase ASCII letters, digits, `.`, `_`, or `-`, and start with a letter or digit"
    )]
    InvalidAlias,
    #[error(
        "service identifier must be nonempty, at most 512 characters, and contain no control characters"
    )]
    InvalidServiceId,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(String);

impl AccountId {
    pub fn generate() -> Self {
        Self(format!("acct_{}", Uuid::now_v7().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AccountId {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(uuid) = value.strip_prefix("acct_") else {
            return Err(IdentifierError::InvalidAccountId);
        };
        let parsed = Uuid::parse_str(uuid).map_err(|_| IdentifierError::InvalidAccountId)?;
        if value != format!("acct_{}", parsed.simple()) {
            return Err(IdentifierError::InvalidAccountId);
        }
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("AccountId").field(&self.0).finish()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for AccountId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountAlias(String);

impl AccountAlias {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AccountAlias {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut characters = value.chars();
        let valid = (1..=64).contains(&value.len())
            && characters.next().is_some_and(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit()
            })
            && characters.all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '.' | '_' | '-')
            });
        valid
            .then(|| Self(value.to_owned()))
            .ok_or(IdentifierError::InvalidAlias)
    }
}

impl fmt::Display for AccountAlias {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for AccountAlias {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AccountAlias {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct OpaqueServiceId(String);

impl OpaqueServiceId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(IdentifierError::InvalidServiceId);
        }
        Ok(Self(value))
    }

    /// Exposes protected metadata only for duplicate detection and service requests.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OpaqueServiceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueServiceId([redacted])")
    }
}

impl<'de> Deserialize<'de> for OpaqueServiceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionPolicy {
    Priority,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutoSelection {
    pub enabled: bool,
    pub policy: SelectionPolicy,
}

impl Default for AutoSelection {
    fn default() -> Self {
        Self {
            enabled: false,
            policy: SelectionPolicy::Priority,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountMetadata {
    pub id: AccountId,
    pub alias: AccountAlias,
    pub auth_mode: AuthMode,
    pub email: Option<String>,
    pub plan_type: Option<PlanType>,
    pub enabled: bool,
    pub priority: u32,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub service_account_id: Option<OpaqueServiceId>,
    pub service_workspace_id: Option<OpaqueServiceId>,
}

impl AccountMetadata {
    pub fn new(alias: AccountAlias, auth_mode: AuthMode, created_at: DateTime<Utc>) -> Self {
        Self {
            id: AccountId::generate(),
            alias,
            auth_mode,
            email: None,
            plan_type: None,
            enabled: true,
            priority: DEFAULT_ACCOUNT_PRIORITY,
            created_at,
            last_used_at: None,
            note: None,
            service_account_id: None,
            service_workspace_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryValidationError {
    #[error("unsupported account registry version {version}")]
    UnsupportedVersion { version: u32 },
    #[error("account registry contains duplicate account id {id}")]
    DuplicateId { id: AccountId },
    #[error("account registry contains duplicate alias {alias}")]
    DuplicateAlias { alias: AccountAlias },
    #[error("accounts {first_id} and {duplicate_id} have the same protected service identity")]
    DuplicateServiceIdentity {
        first_id: AccountId,
        duplicate_id: AccountId,
    },
    #[error("default account {id} is missing")]
    MissingDefault { id: AccountId },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AccountLookupError {
    #[error("unknown account reference `{reference}`")]
    Unknown { reference: String },
    #[error("account reference `{reference}` matches both an alias and an account id")]
    Ambiguous { reference: String },
    #[error("account `{alias}` is disabled")]
    Disabled { id: AccountId, alias: AccountAlias },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountRegistry {
    pub version: u32,
    pub generation: u64,
    pub default_account_id: Option<AccountId>,
    pub auto_selection: AutoSelection,
    pub accounts: Vec<AccountMetadata>,
}

impl Default for AccountRegistry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            generation: 0,
            default_account_id: None,
            auto_selection: AutoSelection::default(),
            accounts: Vec::new(),
        }
    }
}

impl AccountRegistry {
    pub fn validate(&self) -> Result<(), RegistryValidationError> {
        if self.version != REGISTRY_VERSION {
            return Err(RegistryValidationError::UnsupportedVersion {
                version: self.version,
            });
        }
        let mut ids = HashSet::new();
        let mut aliases = HashSet::new();
        let mut service_identities = HashMap::new();
        for account in &self.accounts {
            if !ids.insert(account.id.clone()) {
                return Err(RegistryValidationError::DuplicateId {
                    id: account.id.clone(),
                });
            }
            if !aliases.insert(account.alias.clone()) {
                return Err(RegistryValidationError::DuplicateAlias {
                    alias: account.alias.clone(),
                });
            }
            // Neither service field is established as globally unique. Only the complete pair is
            // sufficient for duplicate detection; partial/shared fields must not reject accounts.
            if let (Some(service_account_id), Some(service_workspace_id)) = (
                account.service_account_id.as_ref(),
                account.service_workspace_id.as_ref(),
            ) && let Some(first_id) = service_identities.insert(
                (service_account_id.clone(), service_workspace_id.clone()),
                account.id.clone(),
            ) {
                return Err(RegistryValidationError::DuplicateServiceIdentity {
                    first_id,
                    duplicate_id: account.id.clone(),
                });
            }
        }
        if let Some(default_id) = &self.default_account_id
            && !ids.contains(default_id)
        {
            return Err(RegistryValidationError::MissingDefault {
                id: default_id.clone(),
            });
        }
        Ok(())
    }

    pub fn add_account(&mut self, account: AccountMetadata) -> Result<(), RegistryValidationError> {
        if self.accounts.iter().any(|item| item.id == account.id) {
            return Err(RegistryValidationError::DuplicateId { id: account.id });
        }
        if self.accounts.iter().any(|item| item.alias == account.alias) {
            return Err(RegistryValidationError::DuplicateAlias {
                alias: account.alias,
            });
        }
        if let (Some(account_service_id), Some(account_workspace_id)) = (
            account.service_account_id.as_ref(),
            account.service_workspace_id.as_ref(),
        ) && let Some(existing) = self.accounts.iter().find(|existing| {
            existing.service_account_id.as_ref() == Some(account_service_id)
                && existing.service_workspace_id.as_ref() == Some(account_workspace_id)
        }) {
            return Err(RegistryValidationError::DuplicateServiceIdentity {
                first_id: existing.id.clone(),
                duplicate_id: account.id,
            });
        }
        self.accounts.push(account);
        Ok(())
    }

    pub fn lookup(&self, reference: &str) -> Result<&AccountMetadata, AccountLookupError> {
        let alias_match = self
            .accounts
            .iter()
            .find(|account| account.alias.as_str() == reference);
        let id_match = reference
            .parse::<AccountId>()
            .ok()
            .and_then(|id| self.accounts.iter().find(|account| account.id == id));
        let account = match (alias_match, id_match) {
            (Some(alias), Some(id)) if alias.id != id.id => {
                return Err(AccountLookupError::Ambiguous {
                    reference: reference.to_owned(),
                });
            }
            (Some(account), _) | (_, Some(account)) => account,
            (None, None) => {
                return Err(AccountLookupError::Unknown {
                    reference: reference.to_owned(),
                });
            }
        };
        if !account.enabled {
            return Err(AccountLookupError::Disabled {
                id: account.id.clone(),
                alias: account.alias.clone(),
            });
        }
        Ok(account)
    }

    /// Returns enabled accounts from highest numeric priority to lowest.
    ///
    /// Equal-priority accounts use their stable account identifier as a deterministic tie-breaker.
    pub fn enabled_by_priority(&self) -> Vec<&AccountMetadata> {
        let mut accounts: Vec<_> = self
            .accounts
            .iter()
            .filter(|account| account.enabled)
            .collect();
        accounts.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        accounts
    }
}
