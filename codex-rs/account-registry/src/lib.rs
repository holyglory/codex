//! Durable, credential-free account profile metadata.

mod migration;
mod model;
mod store;

pub use migration::LegacyMigrationJournal;
pub use migration::LegacyMigrationStage;
pub use migration::MigrationJournalError;
pub use model::AccountAlias;
pub use model::AccountId;
pub use model::AccountLookupError;
pub use model::AccountMetadata;
pub use model::AccountRegistry;
pub use model::AutoSelection;
pub use model::DEFAULT_ACCOUNT_PRIORITY;
pub use model::IdentifierError;
pub use model::OpaqueServiceId;
pub use model::RegistryValidationError;
pub use model::SelectionPolicy;
pub use store::RegistryLockGuard;
pub use store::RegistryStore;
pub use store::RegistryStoreError;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
