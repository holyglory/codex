use std::sync::Arc;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_login::AuthManager;

mod async_scorer;
mod sync_reviewer;

pub use sync_reviewer::install as install_reviewer;

/// Installs the guardian contributors into the extension registry.
pub fn install(
    registry: &mut ExtensionRegistryBuilder<Config>,
    auth_manager: Arc<AuthManager>,
    thread_manager: Weak<ThreadManager>,
) {
    async_scorer::install(registry, auth_manager, thread_manager.clone());
    install_reviewer(registry, thread_manager);
}

/// Installs guardian contributors that resolve authentication from each owning turn.
pub fn install_with_auth_resolver<S, I>(
    registry: &mut ExtensionRegistryBuilder<Config>,
    agent_spawner: S,
    internal_session_spawner: I,
    auth_manager: Arc<AuthManager>,
    auth_resolver: codex_login::SharedProfileAuthRouter,
    thread_manager: Weak<ThreadManager>,
) where
    S: Send + Sync + 'static,
    I: Send + Sync + 'static,
{
    registry.thread_lifecycle_contributor(Arc::new(GuardianExtension::new(agent_spawner)));
    async_scorer::install_with_auth_resolver(
        registry,
        auth_manager,
        auth_resolver,
        thread_manager.clone(),
    );
    sync_reviewer::install(registry, thread_manager, internal_session_spawner);
}
