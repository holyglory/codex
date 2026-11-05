use codex_account_registry::AccountAlias;
use codex_account_registry::RegistryStore;

use super::AccountManagementError;
use super::ManagedAccountSnapshot;
use super::mutate_registry;
use super::resolve_account_id;
use super::snapshot;
use super::summary;
use crate::AuthConfig;

/// Nonsecret profile changes, with the current turn's credential lease preserved.
pub enum ManagedAccountUpdate {
    Rename { account: String, alias: String },
    Enable { account: String },
    Disable { account: String },
    SetDefault { account: String },
    EnableAutomaticSelection,
    DisableAutomaticSelection,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ManagedAccountMetadataMutation {
    pub snapshot: ManagedAccountSnapshot,
    pub account_id: Option<String>,
    pub changed: bool,
}

/// Changes existing profiles under an explicit registry-generation precondition.
pub fn update_managed_account(
    config: &AuthConfig,
    update: ManagedAccountUpdate,
    expected_generation: u64,
) -> Result<ManagedAccountMetadataMutation, AccountManagementError> {
    let store = RegistryStore::new(&config.codex_home);
    let (registry, (account_id, changed)) =
        mutate_registry(&store, Some(expected_generation), |current| {
            let reference = match &update {
                ManagedAccountUpdate::Rename { account, .. }
                | ManagedAccountUpdate::Enable { account }
                | ManagedAccountUpdate::Disable { account }
                | ManagedAccountUpdate::SetDefault { account } => Some(account),
                ManagedAccountUpdate::EnableAutomaticSelection
                | ManagedAccountUpdate::DisableAutomaticSelection => None,
            };
            let id = reference
                .map(|reference| resolve_account_id(current, reference))
                .transpose()?;
            let mut planned = current.clone();
            match &update {
                ManagedAccountUpdate::EnableAutomaticSelection => {
                    planned.auto_selection.enabled = true
                }
                ManagedAccountUpdate::DisableAutomaticSelection => {
                    planned.auto_selection.enabled = false
                }
                ManagedAccountUpdate::Rename { alias, .. } => {
                    let alias = alias
                        .parse::<AccountAlias>()
                        .map_err(|_| AccountManagementError::InvalidUpdate)?;
                    if planned
                        .accounts
                        .iter()
                        .any(|account| account.alias == alias && Some(&account.id) != id.as_ref())
                    {
                        return Err(AccountManagementError::InvalidUpdate);
                    }
                    planned
                        .accounts
                        .iter_mut()
                        .find(|account| Some(&account.id) == id.as_ref())
                        .ok_or(AccountManagementError::UnknownAccount)?
                        .alias = alias;
                }
                ManagedAccountUpdate::SetDefault { .. } => {
                    let account = planned
                        .accounts
                        .iter()
                        .find(|account| Some(&account.id) == id.as_ref())
                        .ok_or(AccountManagementError::UnknownAccount)?;
                    if !account.enabled || !summary(config, &planned, account)?.authenticated {
                        return Err(AccountManagementError::AccountUnavailable);
                    }
                    planned.default_account_id = id.clone();
                }
                ManagedAccountUpdate::Enable { .. } | ManagedAccountUpdate::Disable { .. } => {
                    let enabled = matches!(&update, ManagedAccountUpdate::Enable { .. });
                    planned
                        .accounts
                        .iter_mut()
                        .find(|account| Some(&account.id) == id.as_ref())
                        .ok_or(AccountManagementError::UnknownAccount)?
                        .enabled = enabled;
                    if !enabled && planned.default_account_id == id {
                        planned.default_account_id = None;
                        let mut fallback = None;
                        for candidate in planned.enabled_by_priority() {
                            if summary(config, &planned, candidate)?.authenticated {
                                fallback = Some(candidate.id.clone());
                                break;
                            }
                        }
                        planned.default_account_id = fallback;
                    } else if enabled && planned.default_account_id.is_none() {
                        let account = planned
                            .accounts
                            .iter()
                            .find(|account| Some(&account.id) == id.as_ref())
                            .ok_or(AccountManagementError::UnknownAccount)?;
                        if summary(config, &planned, account)?.authenticated {
                            planned.default_account_id = id.clone();
                        }
                    }
                }
            }
            let changed = &planned != current;
            Ok((planned, (id.map(|id| id.to_string()), changed)))
        })?;
    Ok(ManagedAccountMetadataMutation {
        snapshot: snapshot(config, registry)?,
        account_id,
        changed,
    })
}
