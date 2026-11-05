use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as _;
use thiserror::Error;

use crate::AccountId;

pub const LEGACY_MIGRATION_JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LegacyMigrationStage {
    Prepared,
    LegacyBackupPreserved,
    CredentialStored,
    Verified,
    RegistryStored,
    Completed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyMigrationJournal {
    version: u32,
    target_account_id: AccountId,
    stage: LegacyMigrationStage,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MigrationJournalError {
    #[error("unsupported legacy migration journal version {version}")]
    UnsupportedVersion { version: u32 },
    #[error("legacy migration journal update precedes its start time")]
    UpdateBeforeStart,
    #[error("legacy migration journal timestamps must not move backwards")]
    TimestampRegression,
    #[error("invalid legacy migration transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: LegacyMigrationStage,
        to: LegacyMigrationStage,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyMigrationJournalWire {
    version: u32,
    target_account_id: AccountId,
    stage: LegacyMigrationStage,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'de> Deserialize<'de> for LegacyMigrationJournal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LegacyMigrationJournalWire::deserialize(deserializer)?;
        let journal = Self {
            version: wire.version,
            target_account_id: wire.target_account_id,
            stage: wire.stage,
            started_at: wire.started_at,
            updated_at: wire.updated_at,
        };
        journal.validate().map_err(D::Error::custom)?;
        Ok(journal)
    }
}

impl LegacyMigrationJournal {
    pub fn new(target_account_id: AccountId, started_at: DateTime<Utc>) -> Self {
        Self {
            version: LEGACY_MIGRATION_JOURNAL_VERSION,
            target_account_id,
            stage: LegacyMigrationStage::Prepared,
            started_at,
            updated_at: started_at,
        }
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn target_account_id(&self) -> &AccountId {
        &self.target_account_id
    }

    pub fn stage(&self) -> LegacyMigrationStage {
        self.stage
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    pub fn validate(&self) -> Result<(), MigrationJournalError> {
        if self.version != LEGACY_MIGRATION_JOURNAL_VERSION {
            return Err(MigrationJournalError::UnsupportedVersion {
                version: self.version,
            });
        }
        if self.updated_at < self.started_at {
            return Err(MigrationJournalError::UpdateBeforeStart);
        }
        Ok(())
    }

    pub fn transition(
        &mut self,
        to: LegacyMigrationStage,
        updated_at: DateTime<Utc>,
    ) -> Result<(), MigrationJournalError> {
        use LegacyMigrationStage::*;
        let allowed = matches!(
            (self.stage, to),
            (Prepared, LegacyBackupPreserved)
                | (LegacyBackupPreserved, CredentialStored)
                | (CredentialStored, Verified)
                | (Verified, RegistryStored)
                | (RegistryStored, Completed)
        );
        if !allowed {
            return Err(MigrationJournalError::InvalidTransition {
                from: self.stage,
                to,
            });
        }
        self.set_timestamp(updated_at)?;
        self.stage = to;
        Ok(())
    }

    pub fn rollback(&mut self, updated_at: DateTime<Utc>) -> Result<(), MigrationJournalError> {
        use LegacyMigrationStage::*;
        if matches!(self.stage, Completed | RolledBack) {
            return Err(MigrationJournalError::InvalidTransition {
                from: self.stage,
                to: RolledBack,
            });
        }
        self.set_timestamp(updated_at)?;
        self.stage = RolledBack;
        Ok(())
    }

    fn set_timestamp(&mut self, updated_at: DateTime<Utc>) -> Result<(), MigrationJournalError> {
        if updated_at < self.updated_at {
            return Err(MigrationJournalError::TimestampRegression);
        }
        self.updated_at = updated_at;
        Ok(())
    }
}
