mod policy;
mod world_state;

use std::sync::Arc;
use std::time::Instant;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;

use crate::policy::GitAttributionPolicy;
use crate::policy::GitAttributionRetry;
use crate::policy::POLICY_RETRY_DELAY;
use crate::policy::auth_generation;
use crate::policy::cached_attribution_policy;
use crate::policy::resolve_attribution_policy;
use crate::policy::retry_deferred;
use crate::world_state::git_attribution_world_state_section;

/// Contributes model instructions for agent-created git commits and pull requests.
#[derive(Clone)]
struct GitAttributionExtension {
    auth_manager: Arc<AuthManager>,
    base_url: String,
    http_client_factory: HttpClientFactory,
}

impl ContextContributor for GitAttributionExtension {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let auth_lease = input.turn_store.get::<codex_login::AuthManagerLease>();
            let auth_manager = auth_lease
                .as_ref()
                .map(|lease| Arc::clone(lease.auth_manager()))
                .unwrap_or_else(|| Arc::clone(&self.auth_manager));
            let profile_scoped = auth_lease
                .as_ref()
                .is_some_and(|lease| lease.is_profile_scoped());
            let enabled = loop {
                let current_auth_generation = auth_generation(auth_manager.as_ref());
                let cached = if profile_scoped {
                    input
                        .turn_store
                        .get::<GitAttributionPolicy>()
                        .filter(|policy| policy.auth_generation == current_auth_generation)
                        .map(|policy| policy.as_ref().clone())
                } else {
                    cached_attribution_policy(
                        input.thread_store,
                        input.turn_store,
                        current_auth_generation,
                    )
                };
                let policy = match cached {
                    Some(policy) => policy,
                    None if !profile_scoped
                        && retry_deferred(input.thread_store, current_auth_generation) =>
                    {
                        GitAttributionPolicy {
                            auth_generation: current_auth_generation,
                            enabled: false,
                        }
                    }
                    None => {
                        match resolve_attribution_policy(
                            &auth_manager,
                            &self.base_url,
                            &self.http_client_factory,
                        )
                        .await
                        {
                            Ok(Some(policy)) => {
                                if profile_scoped {
                                    input.turn_store.insert(policy.clone());
                                } else {
                                    input.thread_store.insert(policy.clone());
                                }
                                policy
                            }
                            Ok(None) => {
                                let policy = GitAttributionPolicy {
                                    auth_generation: current_auth_generation,
                                    enabled: false,
                                };
                                input.turn_store.insert(policy.clone());
                                policy
                            }
                            Err(_) => {
                                let auth_generation = auth_generation(auth_manager.as_ref());
                                if !profile_scoped && auth_generation == current_auth_generation {
                                    input.thread_store.insert(GitAttributionRetry {
                                        auth_generation,
                                        retry_at: Instant::now() + POLICY_RETRY_DELAY,
                                    });
                                }
                                GitAttributionPolicy {
                                    auth_generation: current_auth_generation,
                                    enabled: false,
                                }
                            }
                        }
                    }
                };
                if policy.auth_generation == auth_generation(auth_manager.as_ref()) {
                    break policy.enabled;
                }
            };
            vec![git_attribution_world_state_section(enabled)]
        })
    }
}

/// Installs the git-attribution contributor into the extension registry.
pub fn install<C: Sync>(
    registry: &mut ExtensionRegistryBuilder<C>,
    auth_manager: Arc<AuthManager>,
    base_url: String,
    http_client_factory: HttpClientFactory,
) {
    registry.prompt_contributor(Arc::new(GitAttributionExtension {
        auth_manager,
        base_url,
        http_client_factory,
    }));
}

#[cfg(test)]
#[path = "git_attribution_tests.rs"]
mod tests;
