use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_core::config::Config;
use codex_login::ProfileAuthStorage;
use serde::Serialize;

use super::AccountCommandError;
use super::AccountErrorKind;
use super::JSON_SCHEMA_VERSION;
use super::view::print_json;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorJson {
    schema_version: u32,
    healthy: bool,
    generation: Option<u64>,
    checks: Vec<DoctorCheck>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCheck {
    code: &'static str,
    status: &'static str,
}

pub(super) fn run(
    config: &Config,
    store: &RegistryStore,
    json: bool,
) -> Result<(), AccountCommandError> {
    let mut checks = Vec::new();
    let registry = match store.read() {
        Ok(registry) => {
            checks.push(DoctorCheck {
                code: "registryValid",
                status: "ok",
            });
            Some(registry)
        }
        Err(RegistryStoreError::NotFound) => {
            checks.push(DoctorCheck {
                code: "registryMissing",
                status: "error",
            });
            None
        }
        Err(_) => {
            checks.push(DoctorCheck {
                code: "registryInvalid",
                status: "error",
            });
            None
        }
    };
    checks.push(DoctorCheck {
        code: "privatePermissions",
        status: if private_permissions_ok(config, store, registry.as_ref()) {
            "ok"
        } else {
            "error"
        },
    });
    checks.push(DoctorCheck {
        code: "pendingProfileLogin",
        status: if pending_profile_login_present(config) {
            "error"
        } else {
            "ok"
        },
    });
    if let Some(registry) = &registry {
        let active_ok = registry.default_account_id.as_ref().is_none_or(|id| {
            registry
                .accounts
                .iter()
                .any(|account| &account.id == id && account.enabled)
        });
        checks.push(DoctorCheck {
            code: "activeAccountResolution",
            status: if active_ok { "ok" } else { "error" },
        });
        let backend_ok = registry.accounts.iter().all(|account| {
            ProfileAuthStorage::new(
                &config.codex_home,
                account.id.clone(),
                config.cli_auth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
            )
            .and_then(|profile| profile.load())
            .is_ok_and(|auth| auth.is_none_or(|auth| auth.resolved_mode() == account.auth_mode))
        });
        checks.push(DoctorCheck {
            code: "credentialBackends",
            status: if backend_ok { "ok" } else { "error" },
        });
    }
    let healthy = checks.iter().all(|check| check.status == "ok");
    if json {
        print_json(&DoctorJson {
            schema_version: JSON_SCHEMA_VERSION,
            healthy,
            generation: registry.as_ref().map(|registry| registry.generation),
            checks,
        })?;
    } else {
        for check in &checks {
            println!("{}: {}", check.code, check.status);
        }
    }
    if healthy {
        Ok(())
    } else {
        Err(AccountCommandError::new(AccountErrorKind::Integrity))
    }
}

fn private_permissions_ok(
    config: &Config,
    store: &RegistryStore,
    registry: Option<&AccountRegistry>,
) -> bool {
    let private_directory =
        |path: &std::path::Path| codex_private_storage::verify_private_directory(path).is_ok();
    let private_file =
        |path: &std::path::Path| codex_private_storage::verify_private_file(path).is_ok();
    let private_file_if_present = |path: &std::path::Path| !path.exists() || private_file(path);
    let accounts = config.codex_home.join("accounts");
    private_directory(&accounts)
        && private_file(store.registry_path())
        && registry.is_none_or(|registry| {
            registry.accounts.iter().all(|account| {
                let profile = accounts.join(account.id.as_str());
                private_directory(&profile)
                    && private_file_if_present(&profile.join("auth.json"))
                    && private_file_if_present(&profile.join("secrets").join("codex_auth.age"))
            })
        })
}

fn pending_profile_login_present(config: &Config) -> bool {
    let accounts = config.codex_home.join("accounts");
    let entries = match std::fs::read_dir(accounts) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry.file_type().is_ok_and(|file_type| file_type.is_dir())
            && entry.path().join(".pending-profile-login-v1.json").exists()
    })
}
