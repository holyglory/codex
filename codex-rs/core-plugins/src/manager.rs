#[path = "bundled_plugin_exclusions.rs"]
mod bundled_plugin_exclusions;
#[path = "remote_mutations.rs"]
mod remote_mutations;
pub use remote_mutations::RemotePluginInstallOutcome;
pub use remote_mutations::RemotePluginInstallRequest;
pub use remote_mutations::RemotePluginOperationError;
pub use remote_mutations::RemotePluginOperationErrorKind;
pub use remote_mutations::RemotePluginUninstallOutcome;
#[path = "marketplace_context.rs"]
mod marketplace_context;
pub use marketplace_context::PluginMarketplaceContext;
pub use marketplace_context::PluginMarketplaceScope;

use super::LoadedPlugin;
use super::PluginLoadOutcome;
use crate::PluginGitMode;
use crate::app_mcp_routing::apply_app_mcp_routing_policy;
use crate::auth_scope::AuthenticatedPluginJob;
use crate::auth_scope::PluginAuthCacheIdentity;
use crate::auth_scope::PluginAuthIdentity;
use crate::installed_marketplaces::installed_marketplace_roots_from_layer_stack;
use crate::is_openai_curated_marketplace_name;
use crate::loaded_cache_metrics;
use crate::loaded_cache_metrics::RequestOutcome;
use crate::loader::PluginHookLoadOutcome;
use crate::loader::TargetCuratedMarketplace;
use crate::loader::configured_curated_plugin_ids_from_codex_home;
use crate::loader::curated_plugin_cache_version;
use crate::loader::load_plugin_apps_from_manifest;
use crate::loader::load_plugin_hooks;
use crate::loader::load_plugin_hooks_from_layer_stack;
use crate::loader::load_plugin_mcp_servers_from_manifest_with_format;
use crate::loader::load_plugin_skill_inventory;
use crate::loader::load_plugins_from_layer_stack;
use crate::loader::log_plugin_load_errors;
use crate::loader::materialize_marketplace_plugin_source;
use crate::loader::plugin_capability_summary_from_root;
use crate::loader::plugin_is_eligible_for_target_marketplace;
use crate::loader::refresh_curated_plugin_cache;
use crate::loader::refresh_non_curated_plugin_cache_detailed;
use crate::loader::refresh_non_curated_plugin_cache_force_reinstall_detailed;
use crate::loader::remote_installed_plugins_to_config;
use crate::manifest::PluginManifestFormat;
use crate::manifest::PluginManifestInterface;
use crate::manifest::load_plugin_manifest;
use crate::manifest::load_plugin_manifest_with_format;
use crate::marketplace::MarketplaceError;
use crate::marketplace::MarketplaceInterface;
use crate::marketplace::MarketplaceListError;
use crate::marketplace::MarketplaceListOutcome;
use crate::marketplace::MarketplacePluginAuthPolicy;
use crate::marketplace::MarketplacePluginManifestFallback;
use crate::marketplace::MarketplacePluginPolicy;
use crate::marketplace::MarketplacePluginSource;
use crate::marketplace::ResolvedMarketplacePlugin;
use crate::marketplace::find_installable_marketplace_plugin;
use crate::marketplace::find_marketplace_plugin;
use crate::marketplace::home_dir;
use crate::marketplace::list_marketplaces_with_home;
use crate::marketplace::plugin_interface_with_marketplace_category;
use crate::marketplace_policy::MarketplacePolicy;
use crate::marketplace_policy::configured_plugins_from_stack;
use crate::marketplace_upgrade::ConfigLayerReload;
use crate::marketplace_upgrade::ConfiguredMarketplaceUpgradeError;
use crate::marketplace_upgrade::ConfiguredMarketplaceUpgradeOutcome;
use crate::marketplace_upgrade::upgrade_configured_git_marketplaces_with_mode;
use crate::remote::REMOTE_GLOBAL_MARKETPLACE_NAME;
use crate::remote::RecommendedPluginsMode;
use crate::remote::RemoteInstalledPlugin;
use crate::remote::RemoteInstalledPluginBundleSyncError;
use crate::remote::RemoteInstalledPluginBundleSyncOutcome;
use crate::remote::RemotePluginCapabilities;
use crate::remote::RemotePluginCatalogError;
use crate::remote::RemotePluginChange;
use crate::remote::RemotePluginMaterialization;
use crate::remote::RemotePluginScope;
use crate::remote::RemotePluginServiceConfig;
use crate::remote_legacy::RemotePluginFetchError;
use crate::remote_legacy::RemotePluginMutationError;
use crate::remote_plugin_id_resolver::RemoteInstalledPluginsSnapshot;
use crate::remote_plugin_id_resolver::RemotePluginIdResolver;
use crate::remote_plugin_id_resolver::persisted_remote_plugin_id_for_installation;
use crate::skill_snapshots::new_plugin_skill_snapshots;
use crate::startup_sync::OPENAI_PLUGINS_GIT_URL;
use crate::startup_sync::curated_plugins_api_marketplace_path;
use crate::startup_sync::curated_plugins_repo_path;
use crate::startup_sync::read_curated_plugins_sha;
use crate::startup_sync::sync_openai_plugins_repo;
use crate::store::PluginInstallResult as StorePluginInstallResult;
use crate::store::PluginStore;
use crate::store::PluginStoreError;
use crate::store::error_context_sub_error_type;
use crate::tool_suggest_metadata::ToolSuggestMetadataCache;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::PluginInstallSource;
use codex_config::ConfigLayerStack;
use codex_config::SkillConfigRules;
use codex_config::clear_user_plugin;
use codex_config::set_user_plugin_enabled;
use codex_config::skill_config_rules_from_stack;
use codex_config::types::PluginConfig;
use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverableType;
use codex_hooks::plugin_hook_declarations;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_login::AuthManagerLease;
use codex_login::CodexAuth;
use codex_plugin::AppConnectorId;
use codex_plugin::PluginCapabilitySummary;
use codex_plugin::PluginId;
use codex_plugin::PluginIdError;
use codex_plugin::PluginTelemetryMetadata;
use codex_plugin::app_connector_ids_from_declarations;
use codex_plugin::prompt_safe_plugin_description;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::Product;
use codex_skills::SkillMetadata;
use codex_skills::SkillRootLoader;
use codex_skills::SkillRootSnapshots;
use codex_tools::DiscoverablePluginInfo;
use codex_tools::DiscoverableTool;
use codex_tools::filter_request_plugin_install_discoverable_tools_for_client;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::PluginIdentity;
use codex_utils_plugins::PluginSkillRoot;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::OnceCell;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tracing::instrument;
use tracing::warn;

static CURATED_REPO_SYNC_STARTED: AtomicBool = AtomicBool::new(false);
static ACTIVE_REMOTE_BUNDLE_SYNCS: LazyLock<Mutex<HashSet<(PathBuf, PluginAuthCacheIdentity)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
const FEATURED_PLUGIN_IDS_CACHE_TTL: std::time::Duration =
    std::time::Duration::from_secs(60 * 60 * 3);
const REMOTE_INSTALLED_PLUGIN_SYNC_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const LOADED_PLUGINS_CACHE_CAPACITY: usize = 8;

type EffectivePluginsChangedCallback = Arc<dyn Fn(EffectivePluginsChange) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub struct PluginsConfigInput {
    pub config_layer_stack: ConfigLayerStack,
    pub model_provider_id: String,
    pub plugins_enabled: bool,
    pub remote_plugin_enabled: bool,
    pub chatgpt_base_url: String,
    http_client_factory: HttpClientFactory,
}

impl PluginsConfigInput {
    pub fn new(
        config_layer_stack: ConfigLayerStack,
        model_provider_id: String,
        plugins_enabled: bool,
        remote_plugin_enabled: bool,
        chatgpt_base_url: String,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        Self {
            config_layer_stack,
            model_provider_id,
            plugins_enabled,
            remote_plugin_enabled,
            chatgpt_base_url,
            http_client_factory,
        }
    }

    /// Builds route-aware service state for remote plugin requests.
    pub fn remote_plugin_service_config(&self) -> RemotePluginServiceConfig {
        RemotePluginServiceConfig::new(
            self.chatgpt_base_url.clone(),
            self.http_client_factory.clone(),
        )
    }
}

/// Effective-plugin changes that downstream composition layers may act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectivePluginsChange {
    /// Remote bundles installed or updated by background installed-plugin sync.
    pub materialized_remote_plugins: Vec<RemotePluginMaterialization>,
}

/// Inputs used to select endpoint-backed plugin install candidates.
pub struct RecommendedPluginCandidatesInput<'a> {
    pub plugins_config: &'a PluginsConfigInput,
    pub loaded_plugins: &'a PluginLoadOutcome,
    pub auth: Option<&'a CodexAuth>,
    pub disabled_tools: &'a [ToolSuggestDisabledTool],
    pub app_server_client_name: Option<&'a str>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct FeaturedPluginIdsCacheKey {
    chatgpt_base_url: String,
    account_id: Option<String>,
    chatgpt_user_id: Option<String>,
    is_workspace_account: bool,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct RecommendedPluginsCacheKey {
    chatgpt_base_url: String,
    auth_identity: PluginAuthIdentity,
}

#[derive(Clone)]
struct CachedFeaturedPluginIds {
    expires_at: Instant,
    featured_plugin_ids: Vec<String>,
}

#[derive(Default)]
struct RemoteInstalledPluginsCache {
    generation: u64,
    reconciliation_generation: Option<u64>,
    // A cancelled pass may mutate bundles before publication; retry must notify consumers.
    needs_effective_plugins_refresh: bool,
    plugins: Option<Vec<RemoteInstalledPlugin>>,
}

enum RemoteInstalledPluginsCachePublication {
    Refresh,
    Reconcile,
}

/// Holds the cache-root gate shared by full installed-bundle sync, reconciliation, and direct
/// remote plugin mutations.
///
/// Callers should keep this guard only while mutating the remote plugin cache and backend
/// installed state, then release it before unrelated analytics, OAuth, or runtime refresh work.
struct RemoteInstalledPluginSyncGuard {
    _permit: OwnedSemaphorePermit,
}

struct RemoteInstalledPluginsReconciliationGuard<'a> {
    manager: &'a PluginsManager,
    cache_identity: PluginAuthCacheIdentity,
    generation: u64,
    committed: bool,
}

impl Drop for RemoteInstalledPluginsReconciliationGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.manager
                .abandon_remote_installed_plugins_reconcile(&self.cache_identity, self.generation);
        }
    }
}

struct RemoteInstalledPluginsCacheRefreshRequest {
    cache_identity: PluginAuthCacheIdentity,
    generation: u64,
    service_config: RemotePluginServiceConfig,
    auth_job: AuthenticatedPluginJob,
    notify: RemoteInstalledPluginsCacheRefreshNotify,
    // App-server attaches side effects such as skills metadata invalidation and MCP refreshes when
    // remote installed state changes.
    on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    change: EffectivePluginsChange,
}

#[derive(Clone, Copy)]
enum RemoteInstalledPluginsCacheRefreshNotify {
    IfCacheChanged,
    // Remote mutations may change local bundles or active MCP state even when the installed set is
    // unchanged. Notify after `/installed` succeeds so MCP refreshes are ordered after the remote
    // installed cache.
    AfterSuccessfulRefresh,
}

#[derive(Clone)]
enum PluginStartupAuthSource {
    StartupManager(AuthManagerLease),
    Captured(AuthenticatedPluginJob),
}

impl PluginStartupAuthSource {
    fn cached_auth(&self) -> Option<CodexAuth> {
        match self {
            Self::StartupManager(auth_lease) => auth_lease.auth_manager().auth_cached(),
            Self::Captured(auth_job) => auth_job.cloned_auth(),
        }
    }

    async fn resolve(self) -> AuthenticatedPluginJob {
        match self {
            Self::StartupManager(auth_lease) => {
                let auth = auth_lease.auth_manager().auth().await;
                AuthenticatedPluginJob::new(auth_lease, auth)
            }
            Self::Captured(auth_job) => auth_job,
        }
    }
}

#[derive(Default)]
struct RemoteInstalledPluginsCacheRefreshState {
    requested: Option<RemoteInstalledPluginsCacheRefreshRequest>,
    in_flight: bool,
}

struct RemoteCatalogCacheRefreshRequest {
    cache_identity: PluginAuthCacheIdentity,
    service_config: RemotePluginServiceConfig,
    auth_job: AuthenticatedPluginJob,
    scopes: BTreeSet<RemotePluginScope>,
    mode: RemoteCatalogCacheRefreshMode,
}

impl RemoteCatalogCacheRefreshRequest {
    fn has_same_cache_identity(&self, other: &Self) -> bool {
        self.cache_identity == other.cache_identity
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteCatalogCacheRefreshMode {
    OnlyIfStale,
    Force,
}

#[derive(Default)]
struct RemoteCatalogCacheRefreshState {
    requests: VecDeque<RemoteCatalogCacheRefreshRequest>,
    in_flight: bool,
}

struct RemoteInstalledPluginBundleSyncRequest {
    cache_identity: PluginAuthCacheIdentity,
    config: PluginsConfigInput,
    auth_job: AuthenticatedPluginJob,
    on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
}

#[derive(Default)]
struct RemoteInstalledPluginBundleSyncState {
    requested: Option<RemoteInstalledPluginBundleSyncRequest>,
    in_flight: bool,
    active_service_config: Option<RemotePluginServiceConfig>,
    active_has_callback: bool,
}

struct RemoteInstalledPluginBundleSyncRegistration {
    key: (PathBuf, PluginAuthCacheIdentity),
}

impl RemoteInstalledPluginBundleSyncRegistration {
    fn try_register(codex_home: PathBuf, identity: PluginAuthCacheIdentity) -> Option<Self> {
        let key = (codex_home, identity);
        let mut active = ACTIVE_REMOTE_BUNDLE_SYNCS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.insert(key.clone()) {
            Some(Self { key })
        } else {
            None
        }
    }
}

impl Drop for RemoteInstalledPluginBundleSyncRegistration {
    fn drop(&mut self) {
        ACTIVE_REMOTE_BUNDLE_SYNCS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginListBackgroundTaskOptions {
    pub local_marketplaces: Vec<ConfiguredMarketplace>,
    pub remote_catalog_cache_refresh_scopes: BTreeSet<RemotePluginScope>,
}

#[derive(Clone, PartialEq, Eq)]
struct NonCuratedCacheRefreshRequest {
    roots: Vec<AbsolutePathBuf>,
    configured_plugin_keys: Vec<String>,
    configured_plugin_sources: Vec<NonCuratedPluginSource>,
    mode: NonCuratedCacheRefreshMode,
    git_mode: PluginGitMode,
}

#[derive(Clone, PartialEq, Eq)]
struct NonCuratedPluginSource {
    marketplace_path: AbsolutePathBuf,
    plugin_key: String,
    source: MarketplacePluginSource,
    local_version: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NonCuratedCacheRefreshMode {
    IfVersionChanged,
    ForceReinstall,
}

#[derive(Default)]
struct NonCuratedCacheRefreshState {
    requested: Option<NonCuratedCacheRefreshRequest>,
    last_refreshed: Option<NonCuratedCacheRefreshRequest>,
    in_flight: bool,
}

#[derive(Clone, Copy, Default)]
struct NonCuratedCacheRefreshCompletion {
    sequence: u64,
    changed_sequence: u64,
}

#[derive(Default)]
struct ConfiguredMarketplaceUpgradeState {
    in_flight: bool,
}

fn remote_plugin_service_config(config: &PluginsConfigInput) -> RemotePluginServiceConfig {
    config.remote_plugin_service_config()
}

fn featured_plugin_ids_cache_key(
    config: &PluginsConfigInput,
    auth: Option<&CodexAuth>,
) -> FeaturedPluginIdsCacheKey {
    FeaturedPluginIdsCacheKey {
        chatgpt_base_url: config.chatgpt_base_url.clone(),
        account_id: auth.and_then(CodexAuth::get_account_id),
        chatgpt_user_id: auth.and_then(CodexAuth::get_chatgpt_user_id),
        is_workspace_account: auth.is_some_and(CodexAuth::is_workspace_account),
    }
}

fn recommended_plugins_cache_key(
    config: &PluginsConfigInput,
    auth: Option<&CodexAuth>,
) -> RecommendedPluginsCacheKey {
    RecommendedPluginsCacheKey {
        chatgpt_base_url: config.chatgpt_base_url.clone(),
        auth_identity: PluginAuthIdentity::from_auth(auth),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReadRequest {
    pub plugin_name: String,
    pub marketplace_path: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallOutcome {
    pub plugin_id: PluginId,
    pub plugin_version: String,
    pub installed_path: AbsolutePathBuf,
    pub auth_policy: MarketplacePluginAuthPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginReadOutcome {
    pub marketplace_name: String,
    pub marketplace_path: Option<AbsolutePathBuf>,
    pub plugin: PluginDetail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginDetail {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub description: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub installed: bool,
    pub enabled: bool,
    pub skills: Vec<SkillMetadata>,
    pub disabled_skill_paths: HashSet<AbsolutePathBuf>,
    pub hooks: Vec<PluginHookSummary>,
    pub apps: Vec<AppConnectorId>,
    pub app_category_by_id: HashMap<String, String>,
    pub mcp_server_names: Vec<String>,
    pub details_unavailable_reason: Option<PluginDetailsUnavailableReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginHookSummary {
    pub key: String,
    pub event_name: HookEventName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDetailsUnavailableReason {
    InstallRequiredForRemoteSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplace {
    pub name: String,
    pub path: AbsolutePathBuf,
    pub interface: Option<MarketplaceInterface>,
    pub plugins: Vec<ConfiguredMarketplacePlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMarketplacePlugin {
    pub id: String,
    pub name: String,
    pub local_version: Option<String>,
    pub installed_version: Option<String>,
    pub source: MarketplacePluginSource,
    pub policy: MarketplacePluginPolicy,
    pub interface: Option<PluginManifestInterface>,
    pub keywords: Vec<String>,
    pub manifest_fallback: Option<MarketplacePluginManifestFallback>,
    pub installed: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfiguredMarketplaceListOutcome {
    pub marketplaces: Vec<ConfiguredMarketplace>,
    pub errors: Vec<MarketplaceListError>,
}

#[derive(Default)]
struct ConfiguredPluginStates {
    installed: HashSet<String>,
    enabled: HashSet<String>,
}

impl From<PluginDetail> for PluginCapabilitySummary {
    fn from(value: PluginDetail) -> Self {
        let has_skills = value.skills.iter().any(|skill| {
            !value
                .disabled_skill_paths
                .contains(&skill.path_to_skills_md)
        });
        Self {
            config_name: value.id,
            display_name: value.name,
            plugin_namespace: None,
            description: prompt_safe_plugin_description(value.description.as_deref()),
            has_skills,
            mcp_server_names: value.mcp_server_names,
            app_connector_ids: value.apps,
        }
    }
}

pub struct PluginsManager {
    codex_home: PathBuf,
    store: PluginStore,
    featured_plugin_ids_cache: RwLock<HashMap<FeaturedPluginIdsCacheKey, CachedFeaturedPluginIds>>,
    recommended_plugins_cache: RwLock<HashMap<RecommendedPluginsCacheKey, RecommendedPluginsMode>>,
    recommended_plugins_refreshes:
        RwLock<HashMap<RecommendedPluginsCacheKey, Arc<OnceCell<RecommendedPluginsMode>>>>,
    configured_marketplace_upgrade_state: RwLock<ConfiguredMarketplaceUpgradeState>,
    non_curated_cache_refresh_lock: Semaphore,
    non_curated_cache_refresh_state: RwLock<NonCuratedCacheRefreshState>,
    non_curated_cache_refresh_completion: watch::Sender<NonCuratedCacheRefreshCompletion>,
    // Loaded capabilities vary by effective configuration and, for remote plugins, account.
    loaded_plugins_cache: Mutex<LoadedPluginsCache>,
    loaded_plugins_load_semaphore: Semaphore,
    skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    tool_suggest_metadata_cache: ToolSuggestMetadataCache,
    remote_installed_plugins_cache:
        RwLock<HashMap<PluginAuthCacheIdentity, RemoteInstalledPluginsCache>>,
    // TODO(sites-migration): Remove this throttle together with the bundled Sites migration.
    sites_migration_checked_at: Mutex<Option<(PathBuf, Instant)>>,
    remote_installed_plugin_bundle_sync_gate: Arc<Semaphore>,
    remote_installed_plugin_bundle_sync_state:
        RwLock<HashMap<PluginAuthCacheIdentity, RemoteInstalledPluginBundleSyncState>>,
    remote_installed_plugins_cache_refresh_state:
        RwLock<HashMap<PluginAuthCacheIdentity, RemoteInstalledPluginsCacheRefreshState>>,
    remote_catalog_cache_refresh_state:
        RwLock<HashMap<PluginAuthCacheIdentity, RemoteCatalogCacheRefreshState>>,
    restriction_product: Option<Product>,
    auth_manager: Arc<AuthManager>,
    analytics_events_client: RwLock<Option<AnalyticsEventsClient>>,
    plugin_install_source: PluginInstallSource,
}

#[derive(Clone)]
struct LoadedPluginsCacheEntry {
    key: PluginLoadCacheKey,
    plugins: Vec<LoadedPlugin>,
    plugin_skill_snapshots: SkillRootSnapshots<PluginSkillRoot>,
}

#[derive(Default)]
struct LoadedPluginsCache {
    generation: u64,
    // Most recently used first.
    entries: VecDeque<LoadedPluginsCacheEntry>,
}

impl LoadedPluginsCache {
    fn get(&mut self, key: &PluginLoadCacheKey) -> Option<&LoadedPluginsCacheEntry> {
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(index)?;
        self.entries.push_front(entry);
        self.entries.front()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PluginLoadCacheKey {
    configured_plugins: HashMap<String, PluginConfig>,
    skill_config_rules: SkillConfigRules,
    remote_global_catalog_active: bool,
    chatgpt_base_url: String,
    auth_identity: PluginAuthIdentity,
    // TODO(sites-migration): Remove once bundled Sites no longer needs scoped exclusions.
    excluded_plugin_ids: BTreeSet<String>,
}

impl PluginLoadCacheKey {
    fn from_config(
        config: &PluginsConfigInput,
        codex_home: &Path,
        remote_global_catalog_active: bool,
        auth_identity: PluginAuthIdentity,
        excluded_plugin_ids: BTreeSet<String>,
    ) -> Self {
        Self {
            configured_plugins: configured_plugins_from_stack(
                &config.config_layer_stack,
                codex_home,
            ),
            skill_config_rules: skill_config_rules_from_stack(&config.config_layer_stack),
            remote_global_catalog_active,
            chatgpt_base_url: config.chatgpt_base_url.trim_end_matches('/').to_string(),
            auth_identity,
            excluded_plugin_ids,
        }
    }
}

fn target_curated_marketplace(auth_mode: Option<AuthMode>) -> TargetCuratedMarketplace {
    if auth_mode.is_some_and(AuthMode::uses_codex_backend) {
        TargetCuratedMarketplace::OpenAiWithRemote
    } else {
        TargetCuratedMarketplace::OpenAiApi
    }
}

fn projected_auth_mode(auth: Option<&CodexAuth>) -> Option<AuthMode> {
    auth.map(CodexAuth::api_auth_mode)
}

impl PluginsManager {
    pub fn new(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        Self::new_with_options(
            codex_home,
            Some(Product::Codex),
            auth_manager,
            skill_root_loader,
        )
    }

    pub fn new_with_options(
        codex_home: PathBuf,
        restriction_product: Option<Product>,
        auth_manager: Arc<AuthManager>,
        skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>>,
    ) -> Self {
        // Product restrictions are enforced at marketplace admission time for a given CODEX_HOME:
        // listing, install, and curated refresh all consult this restriction context before new
        // plugins enter local config or cache. After admission, runtime plugin loading trusts the
        // contents of that CODEX_HOME and does not re-filter configured plugins by product, so
        // already-admitted plugins may continue exposing MCP servers/tools from shared local state.
        //
        // This assumes a single CODEX_HOME is only used by one product.
        let remote_installed_plugin_bundle_sync_gate =
            crate::remote::remote_installed_plugin_bundle_sync_gate(&codex_home);
        Self {
            codex_home: codex_home.clone(),
            store: PluginStore::new(codex_home),
            featured_plugin_ids_cache: RwLock::new(HashMap::new()),
            recommended_plugins_cache: RwLock::new(HashMap::new()),
            recommended_plugins_refreshes: RwLock::new(HashMap::new()),
            configured_marketplace_upgrade_state: RwLock::new(
                ConfiguredMarketplaceUpgradeState::default(),
            ),
            non_curated_cache_refresh_lock: Semaphore::new(/*permits*/ 1),
            non_curated_cache_refresh_state: RwLock::new(NonCuratedCacheRefreshState::default()),
            non_curated_cache_refresh_completion: watch::channel(
                NonCuratedCacheRefreshCompletion::default(),
            )
            .0,
            loaded_plugins_cache: Mutex::new(LoadedPluginsCache::default()),
            loaded_plugins_load_semaphore: Semaphore::new(/*permits*/ 1),
            skill_root_loader,
            tool_suggest_metadata_cache: ToolSuggestMetadataCache::new(),
            remote_installed_plugins_cache: RwLock::new(HashMap::new()),
            sites_migration_checked_at: Mutex::new(None),
            remote_installed_plugin_bundle_sync_gate,
            remote_installed_plugin_bundle_sync_state: RwLock::new(HashMap::new()),
            remote_installed_plugins_cache_refresh_state: RwLock::new(HashMap::new()),
            remote_catalog_cache_refresh_state: RwLock::new(HashMap::new()),
            restriction_product,
            auth_manager,
            analytics_events_client: RwLock::new(None),
            plugin_install_source: PluginInstallSource::Manual,
        }
    }

    pub fn with_plugin_install_source(mut self, source: PluginInstallSource) -> Self {
        self.plugin_install_source = source;
        self
    }

    pub fn auth_mode(&self) -> Option<AuthMode> {
        self.auth_manager.get_api_auth_mode()
    }

    fn current_auth(&self) -> Option<CodexAuth> {
        self.auth_manager.auth_cached()
    }

    fn legacy_auth_job(&self, auth: Option<CodexAuth>) -> AuthenticatedPluginJob {
        AuthenticatedPluginJob::new(
            AuthManagerLease::legacy(Arc::clone(&self.auth_manager)),
            auth,
        )
    }

    fn remote_global_catalog_active_with_auth(
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> bool {
        config.remote_plugin_enabled && auth.is_some_and(CodexAuth::uses_codex_backend)
    }

    /// Starts the local curated marketplace sync when the remote catalog is unavailable.
    pub fn maybe_start_curated_repo_sync_for_config(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        let auth = self.current_auth();
        self.maybe_start_curated_repo_sync_for_config_with_auth(
            config,
            auth.as_ref(),
            on_effective_plugins_changed,
        );
    }

    pub fn maybe_start_curated_repo_sync_for_config_with_auth(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if config.plugins_enabled
            && !Self::remote_global_catalog_active_with_auth(config, auth)
            && MarketplacePolicy::from_requirements(config.config_layer_stack.requirements())
                .validate_git_source(OPENAI_PLUGINS_GIT_URL, /*ref_name*/ None)
                .is_ok()
        {
            self.start_curated_repo_sync(
                config.http_client_factory.clone(),
                on_effective_plugins_changed,
            );
        }
    }

    pub fn set_analytics_events_client(&self, analytics_events_client: AnalyticsEventsClient) {
        let mut stored_client = match self.analytics_events_client.write() {
            Ok(client_guard) => client_guard,
            Err(err) => err.into_inner(),
        };
        *stored_client = Some(analytics_events_client);
    }

    fn restriction_product_matches(&self, products: Option<&[Product]>) -> bool {
        match products {
            None => true,
            Some([]) => false,
            Some(products) => self
                .restriction_product
                .is_some_and(|product| product.matches_product_restriction(products)),
        }
    }

    pub async fn plugins_for_config(&self, config: &PluginsConfigInput) -> PluginLoadOutcome {
        let auth_changes = self.auth_manager.auth_change_receiver();
        let auth_revision = *auth_changes.borrow();
        let auth = self.current_auth();
        let auth_identity = PluginAuthIdentity::from_auth(auth.as_ref());
        let outcome = self
            .plugins_for_config_with_auth(config, auth.as_ref())
            .await;
        if *auth_changes.borrow() != auth_revision
            && PluginAuthIdentity::from_auth(self.current_auth().as_ref()) != auth_identity
        {
            PluginLoadOutcome::default()
        } else {
            outcome
        }
    }

    pub async fn plugins_for_config_with_auth(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> PluginLoadOutcome {
        self.plugins_for_config_with_force_reload_and_auth(
            config, /*force_reload*/ false, auth,
        )
        .await
    }

    /// Returns skill snapshots parsed while loading the matching plugin cache entry.
    pub fn plugin_skill_snapshots_for_config(
        &self,
        config: &PluginsConfigInput,
    ) -> Option<SkillRootSnapshots<PluginSkillRoot>> {
        let auth = self.current_auth();
        self.plugin_skill_snapshots_for_config_with_auth(config, auth.as_ref())
    }

    /// Returns skill snapshots for the explicitly captured authentication projection.
    pub fn plugin_skill_snapshots_for_config_with_auth(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> Option<SkillRootSnapshots<PluginSkillRoot>> {
        if !config.plugins_enabled {
            return None;
        }
        let key = PluginLoadCacheKey::from_config(
            config,
            self.codex_home.as_path(),
            Self::remote_global_catalog_active_with_auth(config, auth),
            PluginAuthIdentity::from_auth(auth),
            self.excluded_bundled_plugin_ids(config),
        );
        self.loaded_plugins_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|cached| cached.plugin_skill_snapshots.clone())
    }

    #[instrument(
        name = "plugins_for_config",
        level = "info",
        skip_all,
        fields(
            otel.name = "plugins_for_config",
            plugins_enabled = config.plugins_enabled
        )
    )]
    async fn plugins_for_config_with_force_reload_and_auth(
        &self,
        config: &PluginsConfigInput,
        force_reload: bool,
        auth: Option<&CodexAuth>,
    ) -> PluginLoadOutcome {
        if !config.plugins_enabled {
            return PluginLoadOutcome::default();
        }

        let cache_outcome = RequestOutcome::Hit;
        let auth_identity = PluginAuthIdentity::from_auth(auth);
        let remote_global_catalog_active =
            Self::remote_global_catalog_active_with_auth(config, auth);
        let mut cache_key = PluginLoadCacheKey::from_config(
            config,
            self.codex_home.as_path(),
            remote_global_catalog_active,
            auth_identity,
            self.excluded_bundled_plugin_ids(config),
        );
        if !force_reload && let Some(plugins) = self.cached_loaded_plugins(&cache_key) {
            cache_outcome.record();
            return self.resolve_loaded_plugins_for_auth(plugins, auth);
        }

        let wait_started = Instant::now();
        let load_permit = self.loaded_plugins_load_semaphore.acquire().await;
        loaded_cache_metrics::record_duration(
            loaded_cache_metrics::WAIT_DURATION,
            wait_started.elapsed(),
        );
        let Ok(_load_permit) = load_permit else {
            warn!("plugin load semaphore closed");
            return PluginLoadOutcome::default();
        };
        // Migration may have changed the effective exclusion while this load was queued.
        cache_key.excluded_plugin_ids = self.excluded_bundled_plugin_ids(config);
        if !force_reload && let Some(plugins) = self.cached_loaded_plugins(&cache_key) {
            RequestOutcome::HitAfterWait.record();
            return self.resolve_loaded_plugins_for_auth(plugins, auth);
        }
        let cache_generation = self.loaded_plugins_cache_generation();
        let plugin_skill_snapshots = new_plugin_skill_snapshots();
        let load_started = Instant::now();
        let plugins = load_plugins_from_layer_stack(
            &config.config_layer_stack,
            self.remote_installed_plugins_snapshot_with_auth(config, auth),
            &self.store,
            Some(&plugin_skill_snapshots),
            self.restriction_product,
            remote_global_catalog_active,
            self.skill_root_loader.as_ref(),
            &cache_key.excluded_plugin_ids,
        )
        .await;
        loaded_cache_metrics::record_duration(
            loaded_cache_metrics::LOAD_DURATION,
            load_started.elapsed(),
        );
        log_plugin_load_errors(&plugins);
        self.cache_loaded_plugins_if_current(
            cache_generation,
            cache_key,
            plugins.clone(),
            plugin_skill_snapshots,
        );
        RequestOutcome::Load.record();
        self.resolve_loaded_plugins_for_auth(plugins, auth)
    }

    fn resolve_loaded_plugins_for_auth(
        &self,
        mut plugins: Vec<LoadedPlugin>,
        auth: Option<&CodexAuth>,
    ) -> PluginLoadOutcome {
        let auth_mode = projected_auth_mode(auth);
        let target_curated_marketplace = target_curated_marketplace(auth_mode);
        plugins.retain(|plugin| {
            plugin_is_eligible_for_target_marketplace(
                &plugin.config_name,
                target_curated_marketplace,
            )
        });
        for plugin in &mut plugins {
            let plugin_active = plugin.is_active();
            apply_app_mcp_routing_policy(
                &mut plugin.apps,
                &mut plugin.mcp_servers,
                auth_mode,
                plugin_active,
            );
        }
        PluginLoadOutcome::from_plugins(plugins)
    }

    pub fn clear_cache(&self) {
        self.clear_loaded_plugins_cache();
        let mut featured_plugin_ids_cache = match self.featured_plugin_ids_cache.write() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        featured_plugin_ids_cache.clear();
    }

    pub fn clear_recommended_plugins_cache(&self) {
        let mut refreshes = match self.recommended_plugins_refreshes.write() {
            Ok(refreshes) => refreshes,
            Err(err) => err.into_inner(),
        };
        refreshes.clear();
        let mut cache = match self.recommended_plugins_cache.write() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        cache.clear();
    }

    fn clear_loaded_plugins_cache(&self) {
        self.tool_suggest_metadata_cache.clear();
        let mut cache = match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        cache.generation = cache.generation.wrapping_add(1);
        cache.entries.clear();
        drop(cache);
        loaded_cache_metrics::record_event("clear");
    }

    fn clear_loaded_plugins_cache_for_auth(&self, auth_identity: &PluginAuthIdentity) {
        self.tool_suggest_metadata_cache.clear();
        let mut cache = match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        cache.generation = cache.generation.wrapping_add(1);
        cache
            .entries
            .retain(|entry| &entry.key.auth_identity != auth_identity);
        drop(cache);
        loaded_cache_metrics::record_event("clear");
    }

    fn clear_caches_after_marketplace_source_refresh(
        &self,
        installed_plugin_cache_refreshed: bool,
        on_effective_plugins_changed: Option<&EffectivePluginsChangedCallback>,
    ) {
        if installed_plugin_cache_refreshed {
            self.clear_cache();
            if let Some(on_effective_plugins_changed) = on_effective_plugins_changed {
                on_effective_plugins_changed(EffectivePluginsChange::default());
            }
        } else {
            self.tool_suggest_metadata_cache.clear();
        }
    }

    /// Resolve plugin hooks for a config layer stack without loading other plugin capabilities.
    pub async fn plugin_hooks_for_layer_stack(
        &self,
        config_layer_stack: &ConfigLayerStack,
        config: &PluginsConfigInput,
    ) -> PluginHookLoadOutcome {
        let auth = self.current_auth();
        self.plugin_hooks_for_layer_stack_with_auth(config_layer_stack, config, auth.as_ref())
            .await
    }

    pub async fn plugin_hooks_for_layer_stack_with_auth(
        &self,
        config_layer_stack: &ConfigLayerStack,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> PluginHookLoadOutcome {
        if !config.plugins_enabled {
            return PluginHookLoadOutcome::default();
        }
        let target_curated_marketplace = target_curated_marketplace(projected_auth_mode(auth));
        load_plugin_hooks_from_layer_stack(
            config_layer_stack,
            self.remote_installed_plugin_configs_with_auth(config, auth),
            &self.excluded_bundled_plugin_ids(config),
            &self.store,
            target_curated_marketplace,
            Self::remote_global_catalog_active_with_auth(config, auth),
        )
        .await
    }

    fn cached_loaded_plugins(&self, key: &PluginLoadCacheKey) -> Option<Vec<LoadedPlugin>> {
        self.loaded_plugins_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .map(|cached| cached.plugins.clone())
    }

    fn loaded_plugins_cache_generation(&self) -> u64 {
        match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache.generation,
            Err(err) => err.into_inner().generation,
        }
    }

    fn cache_loaded_plugins_if_current(
        &self,
        generation: u64,
        key: PluginLoadCacheKey,
        plugins: Vec<LoadedPlugin>,
        plugin_skill_snapshots: SkillRootSnapshots<PluginSkillRoot>,
    ) {
        let mut cache = match self.loaded_plugins_cache.lock() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        if cache.generation != generation {
            return;
        }
        cache.entries.retain(|entry| entry.key != key);
        cache.entries.push_front(LoadedPluginsCacheEntry {
            key,
            plugins,
            plugin_skill_snapshots,
        });
        let evicted = cache.entries.len() > LOADED_PLUGINS_CACHE_CAPACITY;
        cache.entries.truncate(LOADED_PLUGINS_CACHE_CAPACITY);
        drop(cache);
        if evicted {
            loaded_cache_metrics::record_event("capacity_eviction");
        }
    }

    fn remote_installed_plugin_configs_with_auth(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> HashMap<String, PluginConfig> {
        let cache = match self.remote_installed_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        let Some(plugins) = cache
            .get(&identity)
            .and_then(|entry| entry.plugins.as_ref())
        else {
            return HashMap::new();
        };

        remote_installed_plugins_to_config(plugins, &self.store)
    }

    fn remote_installed_plugins_snapshot_with_auth(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> RemoteInstalledPluginsSnapshot {
        let cache = match self.remote_installed_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        let Some(plugins) = cache
            .get(&identity)
            .and_then(|entry| entry.plugins.as_ref())
        else {
            return RemoteInstalledPluginsSnapshot::default();
        };

        RemoteInstalledPluginsSnapshot {
            configs: remote_installed_plugins_to_config(plugins, &self.store),
            remote_plugin_id_resolver: RemotePluginIdResolver::new(plugins),
        }
    }

    fn remote_plugin_id_for(&self, plugin_id: &PluginId) -> Option<String> {
        let auth = self.current_auth();
        let cache = match self.remote_installed_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let identity = PluginAuthIdentity::from_auth(auth.as_ref());
        let mut matching_caches = cache
            .iter()
            .filter(|(key, _)| key.auth_identity() == &identity)
            .filter_map(|(_, entry)| entry.plugins.as_ref());
        if let Some(plugins) = matching_caches.next()
            && matching_caches.next().is_none()
        {
            return plugins.iter().find_map(|plugin| {
                (plugin.name == plugin_id.plugin_name
                    && plugin.marketplace_name == plugin_id.marketplace_name)
                    .then(|| plugin.id.clone())
            });
        }
        drop(cache);

        let installation = self.store.active_plugin_installation(plugin_id)?;
        persisted_remote_plugin_id_for_installation(&installation)
    }

    fn remote_plugin_id_for_with_auth(
        &self,
        config: &PluginsConfigInput,
        plugin_id: &PluginId,
        auth: Option<&CodexAuth>,
    ) -> Option<String> {
        let cache = match self.remote_installed_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        if let Some(plugins) = cache
            .get(&identity)
            .and_then(|entry| entry.plugins.as_ref())
        {
            return plugins.iter().find_map(|plugin| {
                (plugin.name == plugin_id.plugin_name
                    && plugin.marketplace_name == plugin_id.marketplace_name)
                    .then(|| plugin.id.clone())
            });
        }
        drop(cache);

        let installation = self.store.active_plugin_installation(plugin_id)?;
        persisted_remote_plugin_id_for_installation(&installation)
    }

    pub async fn telemetry_metadata_for_installed_plugin(
        &self,
        plugin_id: &PluginId,
    ) -> PluginTelemetryMetadata {
        let mut metadata = self.telemetry_metadata_for_plugin_id(plugin_id);
        metadata.capability_summary = match self.store.active_plugin_root(plugin_id) {
            Some(plugin_root) => {
                plugin_capability_summary_from_root(
                    plugin_id,
                    &plugin_root,
                    self.skill_root_loader.as_ref(),
                )
                .await
            }
            None => None,
        };
        metadata
    }

    pub async fn telemetry_metadata_for_installed_plugin_with_remote_id(
        &self,
        plugin_id: &PluginId,
        remote_plugin_id: &str,
    ) -> PluginTelemetryMetadata {
        let mut metadata =
            self.telemetry_metadata_for_plugin_id_with_remote_id(plugin_id, remote_plugin_id);
        metadata.capability_summary = match self.store.active_plugin_root(plugin_id) {
            Some(plugin_root) => {
                plugin_capability_summary_from_root(
                    plugin_id,
                    &plugin_root,
                    self.skill_root_loader.as_ref(),
                )
                .await
            }
            None => None,
        };
        metadata
    }

    pub fn telemetry_metadata_for_plugin_id(
        &self,
        plugin_id: &PluginId,
    ) -> PluginTelemetryMetadata {
        PluginTelemetryMetadata {
            plugin_id: Some(plugin_id.clone()),
            remote_plugin_id: self.remote_plugin_id_for(plugin_id),
            capability_summary: None,
        }
    }

    pub fn telemetry_metadata_for_plugin_id_with_remote_id(
        &self,
        plugin_id: &PluginId,
        remote_plugin_id: &str,
    ) -> PluginTelemetryMetadata {
        PluginTelemetryMetadata {
            remote_plugin_id: Some(remote_plugin_id.to_string()),
            ..self.telemetry_metadata_for_plugin_id(plugin_id)
        }
    }

    pub fn telemetry_metadata_for_capability_summary(
        &self,
        summary: &PluginCapabilitySummary,
    ) -> Option<PluginTelemetryMetadata> {
        let plugin_id = PluginId::parse(&summary.config_name).ok()?;
        Some(PluginTelemetryMetadata {
            remote_plugin_id: self.remote_plugin_id_for(&plugin_id),
            plugin_id: Some(plugin_id),
            capability_summary: Some(summary.clone()),
        })
    }

    pub fn build_remote_installed_plugin_marketplaces_from_cache(
        &self,
        visible_marketplaces: &[&str],
    ) -> Option<Vec<crate::remote::RemoteMarketplace>> {
        let auth = self.current_auth();
        let cache = match self.remote_installed_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let identity = PluginAuthIdentity::from_auth(auth.as_ref());
        let mut matching = cache
            .iter()
            .filter(|(key, _)| key.auth_identity() == &identity)
            .filter_map(|(_, entry)| entry.plugins.as_ref());
        let plugins = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        Some(
            crate::remote::group_remote_installed_plugins_by_marketplaces(
                plugins,
                visible_marketplaces,
            ),
        )
    }

    pub fn build_remote_installed_plugin_marketplaces_from_cache_with_auth(
        &self,
        config: &PluginsConfigInput,
        visible_marketplaces: &[&str],
        auth: Option<&CodexAuth>,
    ) -> Option<Vec<crate::remote::RemoteMarketplace>> {
        let cache = match self.remote_installed_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        let plugins = cache.get(&identity)?.plugins.as_ref()?;
        Some(
            crate::remote::group_remote_installed_plugins_by_marketplaces(
                plugins,
                visible_marketplaces,
            ),
        )
    }

    pub fn cached_global_remote_discoverable_plugins_for_config(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> Vec<crate::remote::RemoteDiscoverablePlugin> {
        if !config.plugins_enabled || !config.remote_plugin_enabled {
            return Vec::new();
        }
        let Some(auth) = auth.filter(|auth| auth.uses_codex_backend()) else {
            return Vec::new();
        };
        let Some(account_id) = auth.get_account_id() else {
            return Vec::new();
        };
        if account_id.is_empty() {
            return Vec::new();
        }

        crate::remote::cached_global_remote_discoverable_plugins(
            self.codex_home.as_path(),
            &remote_plugin_service_config(config),
            auth,
        )
    }

    pub async fn build_and_cache_remote_installed_plugin_marketplaces(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
        visible_marketplaces: &[&str],
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) -> Result<Vec<crate::remote::RemoteMarketplace>, RemotePluginCatalogError> {
        let cache_identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        let generation = self.prepare_remote_installed_plugins_cache_generation(&cache_identity);
        let plugins = crate::remote::fetch_remote_installed_plugins(
            &remote_plugin_service_config(config),
            auth,
        )
        .await?;
        let marketplaces = crate::remote::group_remote_installed_plugins_by_marketplaces(
            &plugins,
            visible_marketplaces,
        );
        let Some(changed) = self.write_remote_installed_plugins_cache_snapshot(
            &cache_identity,
            generation,
            plugins,
            auth,
            &config.chatgpt_base_url,
            RemoteInstalledPluginsCachePublication::Refresh,
        ) else {
            return Ok(self
                .build_remote_installed_plugin_marketplaces_from_cache_with_auth(
                    config,
                    visible_marketplaces,
                    auth,
                )
                .unwrap_or(marketplaces));
        };
        if changed && let Some(on_effective_plugins_changed) = on_effective_plugins_changed {
            on_effective_plugins_changed(EffectivePluginsChange::default());
        }
        Ok(self
            .build_remote_installed_plugin_marketplaces_from_cache_with_auth(
                config,
                visible_marketplaces,
                auth,
            )
            .unwrap_or(marketplaces))
    }

    fn prepare_remote_installed_plugins_cache_generation(
        &self,
        identity: &PluginAuthCacheIdentity,
    ) -> u64 {
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        caches.entry(identity.clone()).or_default().generation
    }

    fn begin_remote_installed_plugins_reconcile(
        &self,
        identity: &PluginAuthCacheIdentity,
    ) -> Option<u64> {
        self.prepare_remote_installed_plugins_cache_generation(identity);
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        let cache = caches.get_mut(identity)?;
        cache.generation = cache.generation.wrapping_add(1);
        cache.reconciliation_generation = Some(cache.generation);
        Some(cache.generation)
    }

    fn abandon_remote_installed_plugins_reconcile(
        &self,
        identity: &PluginAuthCacheIdentity,
        generation: u64,
    ) {
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        let Some(cache) = caches.get_mut(identity) else {
            return;
        };
        if cache.reconciliation_generation != Some(generation) {
            return;
        }
        cache.reconciliation_generation = None;
        cache.generation = cache.generation.wrapping_add(1);
        cache.needs_effective_plugins_refresh = true;
        drop(caches);
        self.clear_loaded_plugins_cache_for_auth(identity.auth_identity());
    }

    pub fn clear_remote_installed_plugins_cache(&self) -> bool {
        let auth = self.current_auth();
        let auth_identity = PluginAuthIdentity::from_auth(auth.as_ref());
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        let previous_len = caches.len();
        caches.retain(|identity, _| identity.auth_identity() != &auth_identity);
        if caches.len() == previous_len {
            return false;
        }
        drop(caches);
        self.clear_loaded_plugins_cache_for_auth(&auth_identity);
        true
    }

    fn remote_installed_plugins_cache_generation_if_current(
        &self,
        identity: &PluginAuthCacheIdentity,
    ) -> Option<u64> {
        let caches = match self.remote_installed_plugins_cache.read() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        caches.get(identity).map(|cache| cache.generation)
    }

    fn write_remote_installed_plugins_cache_snapshot(
        &self,
        identity: &PluginAuthCacheIdentity,
        generation: u64,
        plugins: Vec<RemoteInstalledPlugin>,
        auth: Option<&CodexAuth>,
        service_base_url: &str,
        publication: RemoteInstalledPluginsCachePublication,
    ) -> Option<bool> {
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        let cache = caches.get_mut(identity)?;
        if cache.generation != generation {
            return None;
        }
        let is_reconcile = matches!(
            publication,
            RemoteInstalledPluginsCachePublication::Reconcile
        );
        if !is_reconcile && cache.reconciliation_generation == Some(generation) {
            return None;
        }
        if is_reconcile {
            cache.reconciliation_generation = None;
            cache.generation = cache.generation.wrapping_add(1);
        }
        // TODO(sites-migration): Remove exclusion publication once bundled Sites is retired.
        let exclusion_changed = self.update_sites_exclusion(service_base_url, auth, &plugins);
        let needs_effective_plugins_refresh = (is_reconcile
            && std::mem::take(&mut cache.needs_effective_plugins_refresh))
            || exclusion_changed;
        if cache.plugins.as_ref() == Some(&plugins) {
            drop(caches);
            if needs_effective_plugins_refresh {
                self.clear_loaded_plugins_cache_for_auth(identity.auth_identity());
            }
            return Some(needs_effective_plugins_refresh);
        }
        cache.plugins = Some(plugins);
        drop(caches);
        self.clear_loaded_plugins_cache_for_auth(identity.auth_identity());
        Some(true)
    }

    #[cfg(test)]
    fn write_remote_installed_plugins_cache(&self, plugins: Vec<RemoteInstalledPlugin>) -> bool {
        let auth = self.auth_manager.auth_cached();
        let identity = PluginAuthCacheIdentity::new("", auth.as_ref());
        let generation = self.prepare_remote_installed_plugins_cache_generation(&identity);
        self.write_remote_installed_plugins_cache_snapshot(
            &identity,
            generation,
            plugins,
            auth.as_ref(),
            "",
            RemoteInstalledPluginsCachePublication::Refresh,
        )
        .expect("test cache generation should remain current")
    }

    #[cfg(test)]
    fn write_remote_installed_plugins_cache_with_auth(
        &self,
        config: &PluginsConfigInput,
        plugins: Vec<RemoteInstalledPlugin>,
        auth: Option<&CodexAuth>,
    ) -> bool {
        let identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        let generation = self.prepare_remote_installed_plugins_cache_generation(&identity);
        self.write_remote_installed_plugins_cache_snapshot(
            &identity,
            generation,
            plugins,
            auth,
            &config.chatgpt_base_url,
            RemoteInstalledPluginsCachePublication::Refresh,
        )
        .expect("test cache generation should remain current")
    }

    pub fn clear_remote_installed_plugins_cache_with_auth(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> bool {
        let identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        self.clear_remote_installed_plugins_cache_for_identity(&identity)
    }

    fn clear_remote_installed_plugins_cache_for_identity(
        &self,
        identity: &PluginAuthCacheIdentity,
    ) -> bool {
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        if caches.remove(identity).is_none() {
            return false;
        }
        drop(caches);
        self.clear_loaded_plugins_cache_for_auth(identity.auth_identity());
        true
    }

    fn clear_remote_installed_plugins_cache_if_current(
        &self,
        identity: &PluginAuthCacheIdentity,
        generation: u64,
    ) -> Option<bool> {
        let mut caches = match self.remote_installed_plugins_cache.write() {
            Ok(caches) => caches,
            Err(err) => err.into_inner(),
        };
        let cache = caches.get_mut(identity)?;
        if cache.generation != generation {
            return None;
        }
        cache.generation = cache.generation.wrapping_add(1);
        cache.reconciliation_generation = None;
        cache.needs_effective_plugins_refresh = false;
        if cache.plugins.take().is_none() {
            return Some(false);
        }
        drop(caches);
        self.clear_loaded_plugins_cache_for_auth(identity.auth_identity());
        Some(true)
    }

    pub fn maybe_start_remote_plugin_caches_refresh(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        let auth_job = self.legacy_auth_job(auth);
        self.maybe_start_remote_plugin_caches_refresh_with_job(
            config,
            auth_job,
            on_effective_plugins_changed,
        );
    }

    pub fn maybe_start_remote_plugin_caches_refresh_with_auth_lease(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_lease: AuthManagerLease,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_remote_plugin_caches_refresh_with_job(
            config,
            AuthenticatedPluginJob::new(auth_lease, auth),
            on_effective_plugins_changed,
        );
    }

    fn maybe_start_remote_plugin_caches_refresh_with_job(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_job: AuthenticatedPluginJob,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_remote_installed_plugins_cache_refresh_with_notify(
            config,
            auth_job.clone(),
            RemoteInstalledPluginsCacheRefreshNotify::IfCacheChanged,
            on_effective_plugins_changed,
            EffectivePluginsChange::default(),
        );

        let manager = Arc::clone(self);
        let config = config.clone();
        tokio::spawn(async move {
            manager
                .recommended_plugins_mode_for_config(&config, auth_job.auth())
                .await;
        });
    }

    fn maybe_start_remote_installed_plugins_cache_refresh_after_mutation(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        let auth_job = self.legacy_auth_job(auth);
        self.maybe_start_remote_installed_plugins_cache_refresh_after_mutation_with_job(
            config,
            auth_job,
            on_effective_plugins_changed,
        );
    }

    pub fn maybe_start_remote_installed_plugins_cache_refresh_after_mutation_with_auth_lease(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_lease: AuthManagerLease,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_remote_installed_plugins_cache_refresh_after_mutation_with_job(
            config,
            AuthenticatedPluginJob::new(auth_lease, auth),
            on_effective_plugins_changed,
        );
    }

    fn maybe_start_remote_installed_plugins_cache_refresh_after_mutation_with_job(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_job: AuthenticatedPluginJob,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_remote_installed_plugins_cache_refresh_with_notify(
            config,
            auth_job,
            RemoteInstalledPluginsCacheRefreshNotify::AfterSuccessfulRefresh,
            on_effective_plugins_changed,
            EffectivePluginsChange::default(),
        );
    }

    fn maybe_start_remote_installed_plugins_cache_refresh_with_notify(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_job: AuthenticatedPluginJob,
        notify: RemoteInstalledPluginsCacheRefreshNotify,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
        change: EffectivePluginsChange,
    ) {
        if !config.plugins_enabled {
            return;
        }

        let cache_identity = PluginAuthCacheIdentity::from_identity(
            &config.chatgpt_base_url,
            auth_job.identity().clone(),
        );
        let generation = self.prepare_remote_installed_plugins_cache_generation(&cache_identity);
        self.schedule_remote_installed_plugins_cache_refresh(
            RemoteInstalledPluginsCacheRefreshRequest {
                cache_identity,
                generation,
                service_config: remote_plugin_service_config(config),
                auth_job,
                notify,
                on_effective_plugins_changed,
                change,
            },
        );
    }

    pub fn maybe_start_remote_installed_plugin_bundle_sync(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        let auth_job = self.legacy_auth_job(auth);
        self.maybe_start_remote_installed_plugin_bundle_sync_with_job(
            config,
            auth_job,
            on_effective_plugins_changed,
        );
    }

    pub fn maybe_start_remote_installed_plugin_bundle_sync_with_auth_lease(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_lease: AuthManagerLease,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_remote_installed_plugin_bundle_sync_with_job(
            config,
            AuthenticatedPluginJob::new(auth_lease, auth),
            on_effective_plugins_changed,
        );
    }

    fn maybe_start_remote_installed_plugin_bundle_sync_with_job(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_job: AuthenticatedPluginJob,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if !config.plugins_enabled {
            return;
        }
        if auth_job.auth().is_none() {
            return;
        }
        self.schedule_remote_installed_plugin_bundle_sync(RemoteInstalledPluginBundleSyncRequest {
            cache_identity: PluginAuthCacheIdentity::from_identity(
                &config.chatgpt_base_url,
                auth_job.identity().clone(),
            ),
            config: config.clone(),
            auth_job,
            on_effective_plugins_changed,
        });
    }

    /// Acquires the cache-root gate shared by full installed-bundle sync, reconciliation, and
    /// direct remote plugin mutations.
    async fn acquire_remote_installed_plugin_sync_guard(
        &self,
    ) -> Result<RemoteInstalledPluginSyncGuard, RemoteInstalledPluginBundleSyncError> {
        let permit = tokio::time::timeout(
            REMOTE_INSTALLED_PLUGIN_SYNC_WAIT_TIMEOUT,
            Arc::clone(&self.remote_installed_plugin_bundle_sync_gate).acquire_owned(),
        )
        .await
        .map_err(|_| RemoteInstalledPluginBundleSyncError::LockTimeout)?
        .map_err(|_| RemoteInstalledPluginBundleSyncError::Superseded)?;
        Ok(RemoteInstalledPluginSyncGuard { _permit: permit })
    }

    /// Synchronizes remote bundles and publishes the same installed-plugin snapshot.
    ///
    /// The shared gate orders this blocking path with background bundle synchronization. A
    /// reconciled publication advances the cache generation so metadata fetches that started
    /// before or during the bundle pass cannot overwrite its snapshot afterward.
    pub async fn reconcile_remote_installed_plugins(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> Result<RemoteInstalledPluginBundleSyncOutcome, RemoteInstalledPluginBundleSyncError> {
        let _guard = self.acquire_remote_installed_plugin_sync_guard().await?;
        let (outcome, _) = self
            .reconcile_remote_installed_plugins_after_acquiring_gate(config, auth)
            .await?;
        Ok(outcome)
    }

    async fn reconcile_remote_installed_plugins_after_acquiring_gate(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> Result<(RemoteInstalledPluginBundleSyncOutcome, bool), RemoteInstalledPluginBundleSyncError>
    {
        let cache_identity = PluginAuthCacheIdentity::new(&config.chatgpt_base_url, auth);
        let generation = self
            .begin_remote_installed_plugins_reconcile(&cache_identity)
            .ok_or(RemoteInstalledPluginBundleSyncError::Superseded)?;
        let mut reconciliation = RemoteInstalledPluginsReconciliationGuard {
            manager: self,
            cache_identity: cache_identity.clone(),
            generation,
            committed: false,
        };
        let previous_enabled = {
            let cache = match self.remote_installed_plugins_cache.read() {
                Ok(cache) => cache,
                Err(err) => err.into_inner(),
            };
            cache
                .get(&cache_identity)
                .and_then(|entry| entry.plugins.as_ref())
                .map(|plugins| {
                    plugins
                        .iter()
                        .filter_map(|plugin| {
                            PluginId::new(plugin.name.clone(), plugin.marketplace_name.clone())
                                .ok()
                                .map(|plugin_id| (plugin_id, plugin.enabled))
                        })
                        .collect::<HashMap<_, _>>()
                })
        };
        let previous_plugin_ids = previous_enabled
            .iter()
            .flat_map(HashMap::keys)
            .cloned()
            .collect::<Vec<_>>();
        let result = crate::remote::sync_remote_installed_plugin_bundles_once_with_snapshot(
            self.codex_home.clone(),
            &remote_plugin_service_config(config),
            auth,
            &previous_plugin_ids,
        )
        .await?;
        let mut outcome = result.outcome;
        // The generation fence keeps this comparison on the snapshot replaced by this pass.
        // In a known snapshot, absence means inactive: cached reinstalls need the same hints
        // as re-enablement, without becoming materializations or triggering hook-trust writes.
        if let Some(previous_enabled) = previous_enabled {
            for plugin in &result.installed_plugins {
                let plugin_id = PluginId::new(plugin.name.clone(), plugin.marketplace_name.clone())
                    .map_err(|err| RemotePluginCatalogError::UnexpectedResponse(err.to_string()))?;
                if previous_enabled
                    .get(&plugin_id)
                    .copied()
                    .unwrap_or_default()
                    == plugin.enabled
                {
                    continue;
                }
                let plugin_key = plugin_id.as_key();
                if outcome
                    .changed_plugins
                    .iter()
                    .any(|change| change.plugin_id == plugin_key)
                    || self.store.active_plugin_root(&plugin_id).is_none()
                {
                    continue;
                }
                let mut capabilities = RemotePluginCapabilities::default();
                capabilities
                    .include_active_bundle(&self.store, &plugin_id)
                    .await;
                outcome.changed_plugins.push(RemotePluginChange {
                    plugin_id: plugin_key,
                    capabilities,
                });
            }
        }
        outcome
            .changed_plugins
            .sort_unstable_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        let Some(effective_plugins_changed) = self.write_remote_installed_plugins_cache_snapshot(
            &cache_identity,
            generation,
            result.installed_plugins,
            auth,
            &config.chatgpt_base_url,
            RemoteInstalledPluginsCachePublication::Reconcile,
        ) else {
            return Err(RemoteInstalledPluginBundleSyncError::Superseded);
        };
        reconciliation.committed = true;
        if !effective_plugins_changed && outcome.changed_local_cache() {
            self.clear_loaded_plugins_cache_for_auth(cache_identity.auth_identity());
        }
        Ok((outcome, effective_plugins_changed))
    }

    fn maybe_start_remote_catalog_cache_refresh(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        auth_job: AuthenticatedPluginJob,
        scopes: BTreeSet<RemotePluginScope>,
        mode: RemoteCatalogCacheRefreshMode,
    ) {
        if !config.plugins_enabled || scopes.is_empty() {
            return;
        }

        self.schedule_remote_catalog_cache_refresh(RemoteCatalogCacheRefreshRequest {
            cache_identity: PluginAuthCacheIdentity::from_identity(
                &config.chatgpt_base_url,
                auth_job.identity().clone(),
            ),
            service_config: remote_plugin_service_config(config),
            auth_job,
            scopes,
            mode,
        });
    }

    pub fn maybe_start_plugin_list_background_tasks(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
        auth: Option<CodexAuth>,
        options: PluginListBackgroundTaskOptions,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        let auth_job = self.legacy_auth_job(auth);
        self.maybe_start_plugin_list_background_tasks_with_job(
            context,
            auth_job,
            options,
            on_effective_plugins_changed,
        );
    }

    pub fn maybe_start_plugin_list_background_tasks_with_auth_lease(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
        auth_lease: AuthManagerLease,
        auth: Option<CodexAuth>,
        options: PluginListBackgroundTaskOptions,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_plugin_list_background_tasks_with_job(
            context,
            AuthenticatedPluginJob::new(auth_lease, auth),
            options,
            on_effective_plugins_changed,
        );
    }

    fn maybe_start_plugin_list_background_tasks_with_job(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
        auth_job: AuthenticatedPluginJob,
        options: PluginListBackgroundTaskOptions,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_non_curated_plugin_cache_refresh(context, &options.local_marketplaces);
        self.maybe_start_remote_catalog_cache_refresh(
            &context.global_config,
            auth_job.clone(),
            options.remote_catalog_cache_refresh_scopes,
            RemoteCatalogCacheRefreshMode::OnlyIfStale,
        );
        self.maybe_start_remote_plugin_caches_refresh_with_job(
            &context.global_config,
            auth_job.clone(),
            on_effective_plugins_changed.clone(),
        );
        self.maybe_start_remote_installed_plugin_bundle_sync_with_job(
            &context.global_config,
            auth_job,
            on_effective_plugins_changed,
        );
    }

    fn cached_featured_plugin_ids(
        &self,
        cache_key: &FeaturedPluginIdsCacheKey,
    ) -> Option<Vec<String>> {
        {
            let cache = match self.featured_plugin_ids_cache.read() {
                Ok(cache) => cache,
                Err(err) => err.into_inner(),
            };
            let now = Instant::now();
            if let Some(cached) = cache.get(cache_key)
                && now < cached.expires_at
            {
                return Some(cached.featured_plugin_ids.clone());
            }
        }

        let mut cache = match self.featured_plugin_ids_cache.write() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        let now = Instant::now();
        cache.retain(|_, cached| now < cached.expires_at);
        None
    }

    fn write_featured_plugin_ids_cache(
        &self,
        cache_key: FeaturedPluginIdsCacheKey,
        featured_plugin_ids: &[String],
    ) {
        let mut cache = match self.featured_plugin_ids_cache.write() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        cache.insert(
            cache_key,
            CachedFeaturedPluginIds {
                expires_at: Instant::now() + FEATURED_PLUGIN_IDS_CACHE_TTL,
                featured_plugin_ids: featured_plugin_ids.to_vec(),
            },
        );
    }

    pub async fn featured_plugin_ids_for_config(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> Result<Vec<String>, RemotePluginFetchError> {
        if !config.plugins_enabled {
            return Ok(Vec::new());
        }

        let cache_key = featured_plugin_ids_cache_key(config, auth);
        if let Some(featured_plugin_ids) = self.cached_featured_plugin_ids(&cache_key) {
            return Ok(featured_plugin_ids);
        }
        let featured_plugin_ids = crate::remote_legacy::fetch_remote_featured_plugin_ids(
            &remote_plugin_service_config(config),
            auth,
            self.restriction_product,
        )
        .await?;
        self.write_featured_plugin_ids_cache(cache_key, &featured_plugin_ids);
        Ok(featured_plugin_ids)
    }

    #[instrument(
        level = "trace",
        skip_all,
        fields(
            plugins_enabled = config.plugins_enabled,
            remote_plugin_enabled = config.remote_plugin_enabled
        )
    )]
    pub async fn recommended_plugins_mode_for_config(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
    ) -> RecommendedPluginsMode {
        if !config.plugins_enabled
            || !config.remote_plugin_enabled
            || !auth.is_some_and(CodexAuth::uses_codex_backend)
        {
            return RecommendedPluginsMode::Legacy;
        }

        let cache_key = recommended_plugins_cache_key(config, auth);
        if let Some(cached) = self.cached_recommended_plugins_mode(&cache_key) {
            return cached;
        }

        let refresh = {
            let mut refreshes = match self.recommended_plugins_refreshes.write() {
                Ok(refreshes) => refreshes,
                Err(err) => err.into_inner(),
            };
            if let Some(cached) = self.cached_recommended_plugins_mode(&cache_key) {
                return cached;
            }
            refreshes
                .entry(cache_key.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let mode = refresh
            .get_or_init(|| async {
                match crate::remote::fetch_recommended_plugins(
                    &remote_plugin_service_config(config),
                    auth,
                )
                .await
                {
                    Ok(mode) => {
                        let refreshes = match self.recommended_plugins_refreshes.read() {
                            Ok(refreshes) => refreshes,
                            Err(err) => err.into_inner(),
                        };
                        // Hold the refresh lock through publication, matching cache-clear lock
                        // order. An invalidated initializer must not repopulate the cache.
                        if refreshes
                            .get(&cache_key)
                            .is_some_and(|current| Arc::ptr_eq(current, &refresh))
                        {
                            let mut cache = match self.recommended_plugins_cache.write() {
                                Ok(cache) => cache,
                                Err(err) => err.into_inner(),
                            };
                            cache.insert(cache_key.clone(), mode.clone());
                        }
                        mode
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to load recommended plugins");
                        RecommendedPluginsMode::Legacy
                    }
                }
            })
            .await
            .clone();

        let mut refreshes = match self.recommended_plugins_refreshes.write() {
            Ok(refreshes) => refreshes,
            Err(err) => err.into_inner(),
        };
        if refreshes
            .get(&cache_key)
            .is_some_and(|current| Arc::ptr_eq(current, &refresh))
        {
            refreshes.remove(&cache_key);
        }

        mode
    }

    /// Returns endpoint recommendations eligible for installation in the current client.
    /// `None` selects the legacy discovery workflow.
    #[instrument(level = "trace", skip_all)]
    pub async fn recommended_plugin_candidates_for_config(
        &self,
        input: RecommendedPluginCandidatesInput<'_>,
    ) -> Option<Vec<DiscoverableTool>> {
        let RecommendedPluginsMode::Endpoint { plugins } = self
            .recommended_plugins_mode_for_config(input.plugins_config, input.auth)
            .await
        else {
            return None;
        };
        if plugins.is_empty() {
            return Some(Vec::new());
        }

        let installed_plugin_ids = input
            .loaded_plugins
            .plugins()
            .iter()
            .map(|plugin| plugin.config_name.as_str())
            .collect::<HashSet<_>>();
        let installed_remote_plugin_ids = {
            let cache = match self.remote_installed_plugins_cache.read() {
                Ok(cache) => cache,
                Err(err) => err.into_inner(),
            };
            let identity =
                PluginAuthCacheIdentity::new(&input.plugins_config.chatgpt_base_url, input.auth);
            cache
                .get(&identity)
                .and_then(|entry| entry.plugins.as_deref())
                .unwrap_or_default()
                .iter()
                .filter(|plugin| plugin.marketplace_name == REMOTE_GLOBAL_MARKETPLACE_NAME)
                .map(|plugin| plugin.id.clone())
                .collect::<HashSet<_>>()
        };
        let disabled_plugin_ids = input
            .disabled_tools
            .iter()
            .filter(|tool| tool.kind == ToolSuggestDiscoverableType::Plugin)
            .map(|tool| tool.id.as_str())
            .collect::<HashSet<_>>();

        let candidates = plugins
            .into_iter()
            .filter(|plugin| {
                !installed_plugin_ids.contains(plugin.config_id.as_str())
                    && !installed_remote_plugin_ids.contains(plugin.remote_plugin_id.as_str())
                    && !disabled_plugin_ids.contains(plugin.config_id.as_str())
            })
            .map(|plugin| {
                DiscoverableTool::from(DiscoverablePluginInfo {
                    id: plugin.config_id,
                    remote_plugin_id: Some(plugin.remote_plugin_id),
                    name: plugin.display_name,
                    description: None,
                    has_skills: false,
                    mcp_server_names: Vec::new(),
                    app_connector_ids: Vec::new(),
                })
            })
            .collect();
        Some(filter_request_plugin_install_discoverable_tools_for_client(
            candidates,
            input.app_server_client_name,
        ))
    }

    fn cached_recommended_plugins_mode(
        &self,
        cache_key: &RecommendedPluginsCacheKey,
    ) -> Option<RecommendedPluginsMode> {
        let cache = match self.recommended_plugins_cache.read() {
            Ok(cache) => cache,
            Err(err) => err.into_inner(),
        };
        cache.get(cache_key).cloned()
    }

    pub async fn install_plugin(
        &self,
        config: &PluginsConfigInput,
        request: PluginInstallRequest,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let resolved = self.resolve_installable_plugin(config, &request)?;
        let plugin_id = resolved.plugin_id.clone();
        match self.install_resolved_plugin(resolved).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                self.track_plugin_install_failed(
                    &plugin_id,
                    plugin_install_error_type(&err),
                    err.sub_error_type(),
                    err.to_string(),
                );
                Err(err)
            }
        }
    }

    fn resolve_installable_plugin(
        &self,
        config: &PluginsConfigInput,
        request: &PluginInstallRequest,
    ) -> Result<ResolvedMarketplacePlugin, PluginInstallError> {
        let resolved = match find_installable_marketplace_plugin(
            &request.marketplace_path,
            &request.plugin_name,
            self.restriction_product,
        ) {
            Ok(resolved) => resolved,
            Err(err) => {
                self.track_plugin_install_resolution_failed(&err);
                return Err(err.into());
            }
        };
        if self.bundled_sites_is_hidden(config, &resolved.plugin_id.as_key()) {
            let err = MarketplaceError::PluginNotFound {
                plugin_name: resolved.plugin_id.plugin_name,
                marketplace_name: resolved.plugin_id.marketplace_name,
            };
            self.track_plugin_install_resolution_failed(&err);
            return Err(err.into());
        }
        let config_layer_stack = &config.config_layer_stack;
        if let Err(message) =
            MarketplacePolicy::from_requirements(config_layer_stack.requirements())
                .validate_install(
                    config_layer_stack,
                    self.codex_home.as_path(),
                    &request.marketplace_path,
                    &resolved.plugin_id.marketplace_name,
                )
        {
            let err = MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            };
            self.track_plugin_install_resolution_failed(&err);
            return Err(err.into());
        }
        Ok(resolved)
    }

    pub async fn install_plugin_with_remote_sync(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
        request: PluginInstallRequest,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let resolved = self.resolve_installable_plugin(config, &request)?;
        let plugin_id = resolved.plugin_id.as_key();
        // This only forwards the backend mutation before the local install flow.
        if let Err(err) = crate::remote_legacy::enable_remote_plugin(
            &remote_plugin_service_config(config),
            auth,
            &plugin_id,
        )
        .await
        {
            let err = PluginInstallError::from(err);
            self.track_plugin_install_failed(
                &resolved.plugin_id,
                plugin_install_error_type(&err),
                err.sub_error_type(),
                err.to_string(),
            );
            return Err(err);
        }
        let plugin_id = resolved.plugin_id.clone();
        match self.install_resolved_plugin(resolved).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                self.track_plugin_install_failed(
                    &plugin_id,
                    plugin_install_error_type(&err),
                    err.sub_error_type(),
                    err.to_string(),
                );
                Err(err)
            }
        }
    }

    fn track_plugin_install_resolution_failed(&self, err: &MarketplaceError) {
        let sub_error_type = marketplace_error_sub_error_type(err);
        let plugin_id = match err {
            MarketplaceError::PluginNotFound {
                plugin_name,
                marketplace_name,
            }
            | MarketplaceError::PluginNotAvailable {
                plugin_name,
                marketplace_name,
            } => PluginId::new(plugin_name.clone(), marketplace_name.clone()).ok(),
            MarketplaceError::Io { .. }
            | MarketplaceError::MarketplaceNotFound { .. }
            | MarketplaceError::InvalidMarketplaceFile { .. }
            | MarketplaceError::PluginsDisabled
            | MarketplaceError::InvalidPlugin(_) => None,
        };
        if let Some(plugin_id) = plugin_id {
            self.track_plugin_install_failed(
                &plugin_id,
                marketplace_error_type(err),
                sub_error_type,
                err.to_string(),
            );
        } else {
            tracing::warn!(
                error_type = %marketplace_error_type(err),
                sub_error_type = sub_error_type.as_deref(),
                error = %err,
                "plugin install failed while resolving marketplace plugin"
            );
            self.emit_plugin_install_failed(
                PluginTelemetryMetadata {
                    plugin_id: None,
                    remote_plugin_id: None,
                    capability_summary: None,
                },
                marketplace_error_type(err),
                sub_error_type,
            );
        }
    }

    fn track_plugin_install_failed(
        &self,
        plugin_id: &PluginId,
        error_type: &'static str,
        sub_error_type: Option<String>,
        error_message: String,
    ) {
        tracing::warn!(
            plugin_id = %plugin_id.as_key(),
            error_type = %error_type,
            sub_error_type = sub_error_type.as_deref(),
            error = %error_message,
            "plugin install failed"
        );
        self.emit_plugin_install_failed(
            self.telemetry_metadata_for_plugin_id(plugin_id),
            error_type,
            sub_error_type,
        );
    }

    fn emit_plugin_install_failed(
        &self,
        plugin: PluginTelemetryMetadata,
        error_type: &'static str,
        sub_error_type: Option<String>,
    ) {
        let analytics_events_client = match self.analytics_events_client.read() {
            Ok(client) => client.clone(),
            Err(err) => err.into_inner().clone(),
        };
        if let Some(analytics_events_client) = analytics_events_client {
            analytics_events_client.track_plugin_install_failed(
                plugin,
                self.plugin_install_source,
                error_type.to_string(),
                sub_error_type,
            );
        }
    }

    async fn install_resolved_plugin(
        &self,
        resolved: ResolvedMarketplacePlugin,
    ) -> Result<PluginInstallOutcome, PluginInstallError> {
        let auth_policy = resolved.policy.authentication;
        let plugin_version =
            if is_openai_curated_marketplace_name(&resolved.plugin_id.marketplace_name) {
                let curated_plugin_version = read_curated_plugins_sha(self.codex_home.as_path())
                    .ok_or_else(|| {
                        PluginStoreError::Invalid(
                            "local curated marketplace sha is not available".to_string(),
                        )
                    })?;
                Some(curated_plugin_cache_version(&curated_plugin_version))
            } else {
                None
            };
        let store = self.store.clone();
        let codex_home = self.codex_home.clone();
        let manifest_fallback_contents = resolved
            .manifest_fallback
            .contents_if_has_metadata()
            .map(str::to_string);
        let result: StorePluginInstallResult = tokio::task::spawn_blocking(move || {
            let materialized =
                materialize_marketplace_plugin_source(codex_home.as_path(), &resolved.source)
                    .map_err(PluginStoreError::Invalid)?;
            let source_path = materialized.path;
            match (plugin_version, manifest_fallback_contents.as_deref()) {
                (Some(plugin_version), Some(manifest_contents)) => store
                    .install_with_version_and_fallback_manifest(
                        source_path,
                        resolved.plugin_id,
                        plugin_version,
                        manifest_contents,
                    ),
                (Some(plugin_version), None) => {
                    store.install_with_version(source_path, resolved.plugin_id, plugin_version)
                }
                (None, Some(manifest_contents)) => store.install_with_fallback_manifest(
                    source_path,
                    resolved.plugin_id,
                    manifest_contents,
                ),
                (None, None) => store.install(source_path, resolved.plugin_id),
            }
        })
        .await
        .map_err(PluginInstallError::join)??;

        set_user_plugin_enabled(
            &self.codex_home,
            result.plugin_id.as_key(),
            /*enabled*/ true,
        )
        .await
        .map_err(anyhow::Error::from)?;

        let analytics_events_client = match self.analytics_events_client.read() {
            Ok(client) => client.clone(),
            Err(err) => err.into_inner().clone(),
        };
        if let Some(analytics_events_client) = analytics_events_client {
            analytics_events_client.track_plugin_installed(
                self.telemetry_metadata_for_installed_plugin(&result.plugin_id)
                    .await,
            );
        }

        Ok(PluginInstallOutcome {
            plugin_id: result.plugin_id,
            plugin_version: result.plugin_version,
            installed_path: result.installed_path,
            auth_policy,
        })
    }

    pub async fn uninstall_plugin(&self, plugin_id: String) -> Result<(), PluginUninstallError> {
        let plugin_id = PluginId::parse(&plugin_id)?;
        self.uninstall_plugin_id(plugin_id).await
    }

    pub async fn uninstall_plugin_with_remote_sync(
        &self,
        config: &PluginsConfigInput,
        auth: Option<&CodexAuth>,
        plugin_id: String,
    ) -> Result<(), PluginUninstallError> {
        // TODO: Remove this legacy remote-sync path once remote plugins have
        // their own manager and installed-state API.
        let plugin_id = PluginId::parse(&plugin_id)?;
        let plugin_key = plugin_id.as_key();
        // This only forwards the backend mutation before the local uninstall flow.
        crate::remote_legacy::uninstall_remote_plugin(
            &remote_plugin_service_config(config),
            auth,
            &plugin_key,
        )
        .await
        .map_err(PluginUninstallError::from)?;
        self.uninstall_plugin_id(plugin_id).await
    }

    async fn uninstall_plugin_id(&self, plugin_id: PluginId) -> Result<(), PluginUninstallError> {
        let plugin_telemetry = if self.store.active_plugin_root(&plugin_id).is_some() {
            Some(
                self.telemetry_metadata_for_installed_plugin(&plugin_id)
                    .await,
            )
        } else {
            None
        };
        let store = self.store.clone();
        let plugin_id_for_store = plugin_id.clone();
        tokio::task::spawn_blocking(move || store.uninstall(&plugin_id_for_store))
            .await
            .map_err(PluginUninstallError::join)??;

        clear_user_plugin(&self.codex_home, plugin_id.as_key())
            .await
            .map_err(anyhow::Error::from)?;

        let analytics_events_client = match self.analytics_events_client.read() {
            Ok(client) => client.clone(),
            Err(err) => err.into_inner().clone(),
        };
        if let Some(plugin_telemetry) = plugin_telemetry
            && let Some(analytics_events_client) = analytics_events_client
        {
            analytics_events_client.track_plugin_uninstalled(plugin_telemetry);
        }

        Ok(())
    }

    pub fn list_marketplaces_for_context(
        &self,
        context: &PluginMarketplaceContext,
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let auth = self.current_auth();
        self.list_marketplaces_for_context_with_auth(context, include_openai_curated, auth.as_ref())
    }

    pub fn list_marketplaces_for_context_with_auth(
        &self,
        context: &PluginMarketplaceContext,
        include_openai_curated: bool,
        auth: Option<&CodexAuth>,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        context.list_marketplaces(self, include_openai_curated, auth)
    }

    pub fn list_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let auth = self.current_auth();
        self.list_marketplaces_for_config_with_auth(
            config,
            additional_roots,
            include_openai_curated,
            auth.as_ref(),
        )
    }

    pub fn list_marketplaces_for_config_with_auth(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
        auth: Option<&CodexAuth>,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(ConfiguredMarketplaceListOutcome::default());
        }

        let plugin_states = self.configured_plugin_states(config);
        self.list_marketplaces_for_config_with_states(
            config,
            additional_roots,
            include_openai_curated,
            &plugin_states,
            auth,
        )
    }

    fn list_marketplaces_for_config_with_states(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
        plugin_states: &ConfiguredPluginStates,
        auth: Option<&CodexAuth>,
    ) -> Result<ConfiguredMarketplaceListOutcome, MarketplaceError> {
        let excluded_plugin_ids = self.excluded_bundled_plugin_ids(config);
        let marketplace_roots = self.marketplace_roots_with_auth(
            config,
            additional_roots,
            include_openai_curated,
            auth,
        );
        let marketplace_outcome = self.list_marketplaces_with_policy(config, &marketplace_roots)?;
        let mut seen_plugin_keys = HashSet::new();
        let marketplaces = marketplace_outcome
            .marketplaces
            .into_iter()
            .filter_map(|marketplace| {
                let marketplace_name = marketplace.name.clone();
                let plugins = marketplace
                    .plugins
                    .into_iter()
                    .filter_map(|plugin| {
                        let plugin_key = format!("{}@{marketplace_name}", plugin.name);
                        if excluded_plugin_ids.contains(&plugin_key) {
                            return None;
                        }
                        if !seen_plugin_keys.insert(plugin_key.clone()) {
                            return None;
                        }
                        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
                            return None;
                        }
                        let plugin_id =
                            PluginId::new(plugin.name.clone(), marketplace_name.clone()).ok();
                        let installed = plugin_states.installed.contains(&plugin_key);
                        let installed_version = installed.then_some(()).and_then(|_| {
                            plugin_id
                                .as_ref()
                                .and_then(|plugin_id| self.store.active_plugin_version(plugin_id))
                        });
                        let enabled = plugin_states.enabled.contains(&plugin_key);
                        let mut interface = plugin.interface;
                        let mut local_version = plugin.local_version;
                        let manifest_fallback = plugin.manifest_fallback.clone();
                        if installed
                            && plugin.source.is_install_materialized()
                            && let Some(plugin_id) = plugin_id.as_ref()
                            && let Some(plugin_root) = self.store.active_plugin_root(plugin_id)
                            && let Some(manifest) = load_plugin_manifest(plugin_root.as_path())
                        {
                            local_version = manifest.version.clone();
                            let marketplace_category = interface
                                .as_ref()
                                .and_then(|interface| interface.category.clone());
                            interface = plugin_interface_with_marketplace_category(
                                manifest.interface,
                                marketplace_category,
                            );
                        }

                        Some(ConfiguredMarketplacePlugin {
                            // Enabled state is keyed by `<plugin>@<marketplace>`, so duplicate
                            // plugin entries from duplicate marketplace files intentionally
                            // resolve to the first discovered source.
                            id: plugin_key,
                            installed_version,
                            installed,
                            enabled,
                            name: plugin.name,
                            local_version,
                            source: plugin.source,
                            policy: plugin.policy,
                            keywords: plugin.keywords,
                            interface,
                            manifest_fallback,
                        })
                    })
                    .collect::<Vec<_>>();

                (!plugins.is_empty()).then_some(ConfiguredMarketplace {
                    name: marketplace.name,
                    path: marketplace.path,
                    interface: marketplace.interface,
                    plugins,
                })
            })
            .collect();

        Ok(ConfiguredMarketplaceListOutcome {
            marketplaces,
            errors: marketplace_outcome.errors,
        })
    }

    pub fn discover_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        let auth = self.current_auth();
        self.discover_marketplaces_for_config_with_auth(config, additional_roots, auth.as_ref())
    }

    pub fn discover_marketplaces_for_config_with_auth(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        auth: Option<&CodexAuth>,
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Ok(MarketplaceListOutcome::default());
        }

        let marketplace_roots = self.marketplace_roots_with_auth(
            config,
            additional_roots,
            /*include_openai_curated*/ true,
            auth,
        );
        self.list_marketplaces_with_policy(config, &marketplace_roots)
    }

    #[cfg(test)]
    pub(crate) async fn tool_suggest_metadata_for_marketplace_plugin(
        &self,
        marketplace_name: &str,
        plugin: &ConfiguredMarketplacePlugin,
        skill_config_rules: &SkillConfigRules,
    ) -> Result<PluginCapabilitySummary, MarketplaceError> {
        let auth = self.current_auth();
        self.tool_suggest_metadata_for_marketplace_plugin_with_auth(
            marketplace_name,
            plugin,
            skill_config_rules,
            auth.as_ref(),
        )
        .await
    }

    pub(crate) async fn tool_suggest_metadata_for_marketplace_plugin_with_auth(
        &self,
        marketplace_name: &str,
        plugin: &ConfiguredMarketplacePlugin,
        skill_config_rules: &SkillConfigRules,
        auth: Option<&CodexAuth>,
    ) -> Result<PluginCapabilitySummary, MarketplaceError> {
        let fragment = self
            .tool_suggest_metadata_cache
            .metadata_for_plugin(
                marketplace_name,
                plugin,
                self.restriction_product,
                self.skill_root_loader.as_ref(),
            )
            .await?;
        Ok(fragment.project(skill_config_rules, projected_auth_mode(auth)))
    }

    pub async fn read_plugin_for_config(
        &self,
        config: &PluginsConfigInput,
        request: &PluginReadRequest,
    ) -> Result<PluginReadOutcome, MarketplaceError> {
        let auth = self.current_auth();
        self.read_plugin_for_config_with_auth(config, request, auth.as_ref())
            .await
    }

    pub async fn read_plugin_for_config_with_auth(
        &self,
        config: &PluginsConfigInput,
        request: &PluginReadRequest,
        auth: Option<&CodexAuth>,
    ) -> Result<PluginReadOutcome, MarketplaceError> {
        if !config.plugins_enabled {
            return Err(MarketplaceError::PluginsDisabled);
        }

        let plugin = find_marketplace_plugin(&request.marketplace_path, &request.plugin_name)?;
        if self.bundled_sites_is_hidden(config, &plugin.plugin_id.as_key()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.plugin_id.plugin_name,
                marketplace_name: plugin.plugin_id.marketplace_name,
            });
        }
        MarketplacePolicy::from_requirements(config.config_layer_stack.requirements())
            .validate_install(
                &config.config_layer_stack,
                self.codex_home.as_path(),
                &request.marketplace_path,
                &plugin.plugin_id.marketplace_name,
            )
            .map_err(|message| MarketplaceError::InvalidMarketplaceFile {
                path: request.marketplace_path.to_path_buf(),
                message,
            })?;
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.plugin_id.plugin_name,
                marketplace_name: plugin.plugin_id.marketplace_name,
            });
        }

        let marketplace_name = plugin.plugin_id.marketplace_name.clone();
        let plugin_key = plugin.plugin_id.as_key();
        let manifest_fallback = plugin
            .manifest_fallback
            .contents_if_has_metadata()
            .map(|_| plugin.manifest_fallback.clone());
        let plugin_states = self.configured_plugin_states(config);
        let installed = plugin_states.installed.contains(&plugin_key);
        let installed_version = if installed {
            self.store.active_plugin_version(&plugin.plugin_id)
        } else {
            None
        };
        let plugin = self
            .read_plugin_detail_for_marketplace_plugin_with_auth(
                config,
                &marketplace_name,
                ConfiguredMarketplacePlugin {
                    id: plugin_key.clone(),
                    name: plugin.plugin_id.plugin_name,
                    local_version: plugin
                        .manifest
                        .as_ref()
                        .and_then(|manifest| manifest.version.clone()),
                    installed_version,
                    source: plugin.source,
                    policy: plugin.policy,
                    interface: plugin.interface,
                    keywords: plugin
                        .manifest
                        .as_ref()
                        .map(|manifest| manifest.keywords.clone())
                        .unwrap_or_default(),
                    manifest_fallback,
                    installed,
                    enabled: plugin_states.enabled.contains(&plugin_key),
                },
                auth,
            )
            .await?;

        Ok(PluginReadOutcome {
            marketplace_name,
            marketplace_path: Some(request.marketplace_path.clone()),
            plugin,
        })
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn read_plugin_detail_for_marketplace_plugin(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: &str,
        plugin: ConfiguredMarketplacePlugin,
    ) -> Result<PluginDetail, MarketplaceError> {
        let auth = self.current_auth();
        self.read_plugin_detail_for_marketplace_plugin_with_auth(
            config,
            marketplace_name,
            plugin,
            auth.as_ref(),
        )
        .await
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn read_plugin_detail_for_marketplace_plugin_with_auth(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: &str,
        plugin: ConfiguredMarketplacePlugin,
        auth: Option<&CodexAuth>,
    ) -> Result<PluginDetail, MarketplaceError> {
        if !self.restriction_product_matches(plugin.policy.products.as_deref()) {
            return Err(MarketplaceError::PluginNotFound {
                plugin_name: plugin.name,
                marketplace_name: marketplace_name.to_string(),
            });
        }

        let plugin_id =
            PluginId::new(plugin.name.clone(), marketplace_name.to_string()).map_err(|err| {
                match err {
                    PluginIdError::Invalid(message) => MarketplaceError::InvalidPlugin(message),
                }
            })?;
        let plugin_key = plugin_id.as_key();
        if plugin.source.is_install_materialized() && !plugin.installed {
            let description = remote_plugin_install_required_description(&plugin.source);
            return Ok(PluginDetail {
                id: plugin_key,
                name: plugin.name,
                local_version: None,
                description: Some(description),
                source: plugin.source,
                policy: plugin.policy,
                interface: plugin.interface,
                keywords: plugin.keywords,
                installed: plugin.installed,
                enabled: plugin.enabled,
                skills: Vec::new(),
                disabled_skill_paths: HashSet::new(),
                hooks: Vec::new(),
                apps: Vec::new(),
                app_category_by_id: HashMap::new(),
                mcp_server_names: Vec::new(),
                details_unavailable_reason: Some(
                    PluginDetailsUnavailableReason::InstallRequiredForRemoteSource,
                ),
            });
        }

        let source_path = if plugin.source.is_install_materialized() && plugin.installed {
            self.store.active_plugin_root(&plugin_id).ok_or_else(|| {
                MarketplaceError::InvalidPlugin(format!(
                    "installed plugin cache entry is missing for {plugin_key}"
                ))
            })?
        } else {
            let codex_home = self.codex_home.clone();
            let source = plugin.source.clone();
            let materialized = tokio::task::spawn_blocking(move || {
                materialize_marketplace_plugin_source(codex_home.as_path(), &source)
            })
            .await
            .map_err(|err| {
                MarketplaceError::InvalidPlugin(format!(
                    "failed to materialize plugin source: {err}"
                ))
            })?
            .map_err(MarketplaceError::InvalidPlugin)?;
            materialized.path.clone()
        };
        if !source_path.as_path().is_dir() {
            return Err(MarketplaceError::InvalidPlugin(
                "path does not exist or is not a directory".to_string(),
            ));
        }
        let loaded_manifest =
            if codex_utils_plugins::find_plugin_manifest_path(source_path.as_path()).is_some() {
                load_plugin_manifest_with_format(source_path.as_path())
            } else {
                plugin
                    .manifest_fallback
                    .as_ref()
                    .and_then(|fallback| fallback.parse_for_plugin_root(source_path.as_path()))
                    .map(|manifest| crate::manifest::LoadedPluginManifest {
                        manifest,
                        format: PluginManifestFormat::Legacy,
                    })
            }
            .ok_or_else(|| {
                MarketplaceError::InvalidPlugin("missing or invalid plugin.json".to_string())
            })?;
        let manifest_format = loaded_manifest.format;
        let manifest = loaded_manifest.manifest;
        let description = manifest.description.clone();
        let marketplace_category = plugin
            .interface
            .as_ref()
            .and_then(|interface| interface.category.clone());
        let interface = plugin_interface_with_marketplace_category(
            manifest.interface.clone(),
            marketplace_category,
        );
        let plugin_identity = PluginIdentity {
            plugin_id: plugin_id.as_key(),
            remote_plugin_id: self.remote_plugin_id_for_with_auth(config, &plugin_id, auth),
        };
        let skill_config_rules = skill_config_rules_from_stack(&config.config_layer_stack);
        let resolved_skills = load_plugin_skill_inventory(
            &source_path,
            &plugin_identity,
            &manifest,
            manifest_format,
            self.restriction_product,
            /*plugin_skill_snapshots*/ None,
            self.skill_root_loader.as_ref(),
        )
        .await
        .resolve(&skill_config_rules);
        let plugin_data_root = self.store.plugin_data_root(&plugin_id);
        let (hook_sources, _hook_load_warnings) = if manifest_format == PluginManifestFormat::Legacy
        {
            load_plugin_hooks(&source_path, &plugin_id, &plugin_data_root, &manifest.paths)
        } else {
            (Vec::new(), Vec::new())
        };
        let hooks = plugin_hook_declarations(&hook_sources)
            .into_iter()
            .map(|hook| PluginHookSummary {
                key: hook.key,
                event_name: hook.event_name,
            })
            .collect();
        let auth_mode = projected_auth_mode(auth);
        let mut app_declarations = if manifest_format == PluginManifestFormat::Legacy {
            load_plugin_apps_from_manifest(source_path.as_path(), &manifest.paths).await
        } else {
            Vec::new()
        };
        let mcp_data_root = (manifest_format == PluginManifestFormat::AgentPlugin)
            .then(|| self.store.mcp_data_root(&plugin_id, manifest_format));
        let mut mcp_servers = load_plugin_mcp_servers_from_manifest_with_format(
            source_path.as_path(),
            &manifest.paths,
            /*plugin_policy*/ None,
            mcp_data_root.as_deref(),
            manifest_format,
        )
        .await;
        if manifest_format == PluginManifestFormat::Legacy {
            apply_app_mcp_routing_policy(
                &mut app_declarations,
                &mut mcp_servers,
                auth_mode,
                /*plugin_active*/ true,
            );
        }
        let apps = app_connector_ids_from_declarations(&app_declarations);
        let mut seen_app_connector_ids = HashSet::new();
        let mut app_category_by_id = HashMap::new();
        for app in &app_declarations {
            if seen_app_connector_ids.insert(app.connector_id.0.as_str())
                && let Some(category) = &app.category
            {
                app_category_by_id.insert(app.connector_id.0.clone(), category.clone());
            }
        }
        let mut mcp_server_names = mcp_servers.into_keys().collect::<Vec<_>>();
        mcp_server_names.sort_unstable();
        mcp_server_names.dedup();

        Ok(PluginDetail {
            id: plugin.id,
            name: plugin.name,
            local_version: manifest.version.clone(),
            description,
            source: plugin.source,
            policy: plugin.policy,
            interface,
            keywords: manifest.keywords,
            installed: plugin.installed,
            enabled: plugin.enabled,
            skills: resolved_skills.skills,
            disabled_skill_paths: resolved_skills.disabled_skill_paths,
            hooks,
            apps,
            app_category_by_id,
            mcp_server_names,
            details_unavailable_reason: None,
        })
    }

    pub fn maybe_start_plugin_startup_tasks_for_config(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        reload_config: ConfigLayerReload,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        let auth_lease = AuthManagerLease::legacy(Arc::clone(&self.auth_manager));
        self.maybe_start_plugin_startup_tasks_for_config_with_auth_source(
            config,
            reload_config,
            PluginStartupAuthSource::StartupManager(auth_lease),
            on_effective_plugins_changed,
        );
    }

    pub fn maybe_start_plugin_startup_tasks_for_config_with_captured_auth(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        reload_config: ConfigLayerReload,
        auth_lease: AuthManagerLease,
        auth: Option<CodexAuth>,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        self.maybe_start_plugin_startup_tasks_for_config_with_auth_source(
            config,
            reload_config,
            PluginStartupAuthSource::Captured(AuthenticatedPluginJob::new(auth_lease, auth)),
            on_effective_plugins_changed,
        );
    }

    fn maybe_start_plugin_startup_tasks_for_config_with_auth_source(
        self: &Arc<Self>,
        config: &PluginsConfigInput,
        reload_config: ConfigLayerReload,
        auth_source: PluginStartupAuthSource,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if config.plugins_enabled {
            let startup_auth = auth_source.cached_auth();
            self.maybe_start_curated_repo_sync_for_config_with_auth(
                config,
                startup_auth.as_ref(),
                on_effective_plugins_changed.clone(),
            );
            let should_spawn_marketplace_auto_upgrade = {
                let mut state = match self.configured_marketplace_upgrade_state.write() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                if state.in_flight {
                    false
                } else {
                    state.in_flight = true;
                    true
                }
            };
            if should_spawn_marketplace_auto_upgrade {
                let manager = Arc::clone(self);
                let config = config.clone();
                let on_effective_plugins_changed = on_effective_plugins_changed.clone();
                let runtime = tokio::runtime::Handle::current();
                if let Err(err) = std::thread::Builder::new()
                    .name("plugins-marketplace-auto-upgrade".to_string())
                    .spawn(move || {
                        let outcome = manager.upgrade_configured_marketplaces_for_config_with_mode(
                            &config,
                            /*marketplace_name*/ None,
                            PluginGitMode::Automatic,
                            &reload_config,
                        );
                        match outcome {
                            Ok(outcome) => {
                                if !outcome.upgraded_roots.is_empty()
                                    && let Some(on_effective_plugins_changed) =
                                        on_effective_plugins_changed
                                {
                                    runtime.spawn(async move {
                                        on_effective_plugins_changed(
                                            EffectivePluginsChange::default(),
                                        );
                                    });
                                }
                                for error in outcome.errors {
                                    warn!(
                                        marketplace = error.marketplace_name,
                                        error = %error.message,
                                        "failed to auto-upgrade configured marketplace"
                                    );
                                }
                            }
                            Err(err) => {
                                warn!("failed to auto-upgrade configured marketplaces: {err}");
                            }
                        }

                        let mut state = match manager.configured_marketplace_upgrade_state.write() {
                            Ok(state) => state,
                            Err(err) => err.into_inner(),
                        };
                        state.in_flight = false;
                    })
                {
                    let mut state = match self.configured_marketplace_upgrade_state.write() {
                        Ok(state) => state,
                        Err(err) => err.into_inner(),
                    };
                    state.in_flight = false;
                    warn!("failed to start configured marketplace auto-upgrade task: {err}");
                }
            }
            let config_for_remote_sync = config.clone();
            let manager = Arc::clone(self);
            let remote_auth_source = auth_source.clone();
            let on_effective_plugins_changed = on_effective_plugins_changed.clone();
            tokio::spawn(async move {
                let auth_job = remote_auth_source.resolve().await;
                manager.maybe_start_remote_plugin_caches_refresh_with_job(
                    &config_for_remote_sync,
                    auth_job.clone(),
                    on_effective_plugins_changed.clone(),
                );
                manager.maybe_start_remote_installed_plugin_bundle_sync_with_job(
                    &config_for_remote_sync,
                    auth_job.clone(),
                    on_effective_plugins_changed,
                );
                let mut scopes = crate::remote::cached_remote_plugin_catalog_scopes(
                    manager.codex_home.as_path(),
                    &remote_plugin_service_config(&config_for_remote_sync),
                    auth_job.auth(),
                );
                if config_for_remote_sync.remote_plugin_enabled {
                    scopes.insert(RemotePluginScope::Global);
                } else {
                    scopes.retain(|scope| *scope == RemotePluginScope::Workspace);
                }
                manager.maybe_start_remote_catalog_cache_refresh(
                    &config_for_remote_sync,
                    auth_job,
                    scopes,
                    RemoteCatalogCacheRefreshMode::Force,
                );
            });

            let config_for_featured_plugins = config.clone();
            let manager = Arc::clone(self);
            tokio::spawn(async move {
                let auth_job = auth_source.resolve().await;
                if let Err(err) = manager
                    .featured_plugin_ids_for_config(&config_for_featured_plugins, auth_job.auth())
                    .await
                {
                    warn!(
                        error = %err,
                        "failed to warm featured plugin ids cache"
                    );
                }
            });
        }
    }

    pub fn upgrade_configured_marketplaces_for_config(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: Option<&str>,
        reload_config: &ConfigLayerReload,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        self.upgrade_configured_marketplaces_for_config_with_mode(
            config,
            marketplace_name,
            PluginGitMode::Manual,
            reload_config,
        )
    }

    /// Carries the initiating Git trust policy through marketplace and plugin refreshes.
    fn upgrade_configured_marketplaces_for_config_with_mode(
        &self,
        config: &PluginsConfigInput,
        marketplace_name: Option<&str>,
        mode: PluginGitMode,
        reload_config: &ConfigLayerReload,
    ) -> Result<ConfiguredMarketplaceUpgradeOutcome, String> {
        let mut outcome = upgrade_configured_git_marketplaces_with_mode(
            self.codex_home.as_path(),
            &config.config_layer_stack,
            marketplace_name,
            mode,
            reload_config,
        );
        if let Some(marketplace_name) = marketplace_name
            && outcome.selected_marketplaces.is_empty()
        {
            return Err(format!(
                "marketplace `{marketplace_name}` is not configured as a Git marketplace"
            ));
        }
        if !outcome.upgraded_roots.is_empty() {
            let mut configured_plugin_keys = configured_plugins_from_stack(
                &config.config_layer_stack,
                self.codex_home.as_path(),
            )
            .into_keys()
            .collect::<Vec<_>>();
            configured_plugin_keys.sort_unstable();
            match refresh_non_curated_plugin_cache_force_reinstall_detailed(
                self.codex_home.as_path(),
                &outcome.upgraded_roots,
                &configured_plugin_keys,
                mode,
            ) {
                Ok(refresh_outcome) => {
                    self.clear_caches_after_marketplace_source_refresh(
                        refresh_outcome.cache_refreshed,
                        /*on_effective_plugins_changed*/ None,
                    );
                    outcome
                        .errors
                        .extend(refresh_outcome.errors.into_iter().map(|error| {
                            ConfiguredMarketplaceUpgradeError {
                                marketplace_name: error.marketplace_name,
                                message: error.message,
                            }
                        }));
                }
                Err(err) => {
                    self.clear_cache();
                    outcome.errors.push(ConfiguredMarketplaceUpgradeError {
                        marketplace_name: marketplace_name
                            .unwrap_or("all configured marketplaces")
                            .to_string(),
                        message: format!(
                            "failed to refresh installed plugin cache after marketplace upgrade: {err}"
                        ),
                    });
                }
            }
        }
        Ok(outcome)
    }

    pub fn maybe_start_non_curated_plugin_cache_refresh(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
        marketplaces: &[ConfiguredMarketplace],
    ) {
        if let Some(request) = context.non_curated_cache_refresh_request(
            self,
            marketplaces,
            NonCuratedCacheRefreshMode::IfVersionChanged,
            PluginGitMode::Automatic,
        ) {
            self.schedule_non_curated_plugin_cache_refresh(request);
        }
    }

    /// Runs an explicitly requested refresh using the caller's normal Git configuration.
    pub async fn refresh_non_curated_plugin_cache_for_context(
        self: &Arc<Self>,
        context: &PluginMarketplaceContext,
    ) -> bool {
        let Ok(_refresh_permit) = self.non_curated_cache_refresh_lock.acquire().await else {
            return false;
        };
        let mut completion = self.non_curated_cache_refresh_completion.subscribe();
        let changed_sequence = completion.borrow_and_update().changed_sequence;
        let marketplaces =
            match self.list_marketplaces_for_context(context, /*include_openai_curated*/ false) {
                Ok(outcome) => outcome.marketplaces,
                Err(err) => {
                    warn!("failed to prepare non-curated plugin cache refresh: {err}");
                    Vec::new()
                }
            };
        if let Some(request) = context.non_curated_cache_refresh_request(
            self,
            &marketplaces,
            NonCuratedCacheRefreshMode::IfVersionChanged,
            PluginGitMode::Manual,
        ) {
            self.schedule_non_curated_plugin_cache_refresh(request);
        }

        loop {
            let in_flight = match self.non_curated_cache_refresh_state.read() {
                Ok(state) => state.in_flight,
                Err(err) => err.into_inner().in_flight,
            };
            if !in_flight {
                return completion.borrow().changed_sequence != changed_sequence;
            }
            if completion.changed().await.is_err() {
                return false;
            }
        }
    }

    fn schedule_remote_installed_plugins_cache_refresh(
        self: &Arc<Self>,
        mut request: RemoteInstalledPluginsCacheRefreshRequest,
    ) {
        let cache_identity = request.cache_identity.clone();
        if self.remote_installed_plugins_cache_generation_if_current(&cache_identity)
            != Some(request.generation)
        {
            return;
        }
        let should_spawn = {
            let mut states = match self.remote_installed_plugins_cache_refresh_state.write() {
                Ok(states) => states,
                Err(err) => err.into_inner(),
            };
            let state = states.entry(cache_identity.clone()).or_default();
            if self.remote_installed_plugins_cache_generation_if_current(&cache_identity)
                != Some(request.generation)
            {
                return;
            }
            if let Some(existing_request) = state.requested.as_ref()
                && existing_request.generation == request.generation
            {
                if matches!(
                    existing_request.notify,
                    RemoteInstalledPluginsCacheRefreshNotify::AfterSuccessfulRefresh
                ) {
                    request.notify =
                        RemoteInstalledPluginsCacheRefreshNotify::AfterSuccessfulRefresh;
                }
                if !existing_request
                    .change
                    .materialized_remote_plugins
                    .is_empty()
                    && let Some(existing_callback) =
                        existing_request.on_effective_plugins_changed.as_ref()
                {
                    request.on_effective_plugins_changed = Some(Arc::clone(existing_callback));
                } else if request.on_effective_plugins_changed.is_none() {
                    request.on_effective_plugins_changed =
                        existing_request.on_effective_plugins_changed.clone();
                }
                for materialization in &existing_request.change.materialized_remote_plugins {
                    if !request
                        .change
                        .materialized_remote_plugins
                        .iter()
                        .any(|pending| pending.plugin_id == materialization.plugin_id)
                    {
                        request
                            .change
                            .materialized_remote_plugins
                            .push(materialization.clone());
                    }
                }
                request
                    .change
                    .materialized_remote_plugins
                    .sort_by_key(|materialization| materialization.plugin_id.as_key());
            }
            state.requested = Some(request);
            if state.in_flight {
                false
            } else {
                state.in_flight = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager
                .run_remote_installed_plugins_cache_refresh_loop(cache_identity)
                .await;
        });
    }

    fn schedule_remote_installed_plugin_bundle_sync(
        self: &Arc<Self>,
        mut request: RemoteInstalledPluginBundleSyncRequest,
    ) {
        let cache_identity = request.cache_identity.clone();
        let should_spawn = {
            let mut states = match self.remote_installed_plugin_bundle_sync_state.write() {
                Ok(states) => states,
                Err(err) => err.into_inner(),
            };
            let state = states.entry(cache_identity.clone()).or_default();
            let service_config = remote_plugin_service_config(&request.config);
            let duplicates_active = state.in_flight
                && state.requested.is_none()
                && state.active_service_config.as_ref() == Some(&service_config)
                && (request.on_effective_plugins_changed.is_none() || state.active_has_callback);
            if duplicates_active {
                false
            } else {
                if request.on_effective_plugins_changed.is_none()
                    && let Some(pending) = state.requested.as_ref()
                {
                    request.on_effective_plugins_changed =
                        pending.on_effective_plugins_changed.clone();
                }
                state.requested = Some(request);
                if state.in_flight {
                    false
                } else {
                    state.in_flight = true;
                    true
                }
            }
        };
        if !should_spawn {
            return;
        }

        let Some(registration) = RemoteInstalledPluginBundleSyncRegistration::try_register(
            self.codex_home.clone(),
            cache_identity.clone(),
        ) else {
            let mut states = self
                .remote_installed_plugin_bundle_sync_state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(state) = states.get_mut(&cache_identity) {
                state.requested = None;
                state.in_flight = false;
            }
            return;
        };

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager
                .run_remote_installed_plugin_bundle_sync_loop(cache_identity, registration)
                .await;
        });
    }

    fn schedule_remote_catalog_cache_refresh(
        self: &Arc<Self>,
        request: RemoteCatalogCacheRefreshRequest,
    ) {
        let cache_identity = request.cache_identity.clone();
        let should_spawn = {
            let mut states = match self.remote_catalog_cache_refresh_state.write() {
                Ok(states) => states,
                Err(err) => err.into_inner(),
            };
            let state = states.entry(cache_identity.clone()).or_default();
            if let Some(pending) = state
                .requests
                .iter_mut()
                .find(|pending| pending.has_same_cache_identity(&request))
            {
                pending.scopes.extend(request.scopes);
                pending.auth_job = request.auth_job;
                pending.mode = match (pending.mode, request.mode) {
                    (RemoteCatalogCacheRefreshMode::Force, _)
                    | (_, RemoteCatalogCacheRefreshMode::Force) => {
                        RemoteCatalogCacheRefreshMode::Force
                    }
                    (
                        RemoteCatalogCacheRefreshMode::OnlyIfStale,
                        RemoteCatalogCacheRefreshMode::OnlyIfStale,
                    ) => RemoteCatalogCacheRefreshMode::OnlyIfStale,
                };
            } else {
                state.requests.push_back(request);
            }
            if state.in_flight {
                false
            } else {
                state.in_flight = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager
                .run_remote_catalog_cache_refresh_loop(cache_identity)
                .await;
        });
    }

    fn schedule_non_curated_plugin_cache_refresh(
        self: &Arc<Self>,
        mut request: NonCuratedCacheRefreshRequest,
    ) {
        let should_spawn = {
            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            if request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && state.last_refreshed.as_ref().is_some_and(|last_refreshed| {
                    request.configured_plugin_sources.iter().any(|source| {
                        last_refreshed
                            .configured_plugin_sources
                            .iter()
                            .any(|previous_source| {
                                previous_source.plugin_key == source.plugin_key
                                    && previous_source.local_version == source.local_version
                                    && (previous_source.marketplace_path != source.marketplace_path
                                        || previous_source.source != source.source)
                            })
                    })
                })
            {
                request.mode = NonCuratedCacheRefreshMode::ForceReinstall;
            }
            if request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && state.requested.as_ref().is_some_and(|requested| {
                    requested.mode == NonCuratedCacheRefreshMode::ForceReinstall
                        && requested.roots == request.roots
                })
            {
                request.mode = NonCuratedCacheRefreshMode::ForceReinstall;
            }
            // Reconcile each canonical plugin generation once before publishing its resource.
            if state.requested.as_ref().is_some_and(|requested| {
                requested == &request
                    || (requested.git_mode == PluginGitMode::Manual
                        && request.git_mode == PluginGitMode::Automatic
                        && requested.roots == request.roots
                        && requested.configured_plugin_keys == request.configured_plugin_keys
                        && requested.configured_plugin_sources == request.configured_plugin_sources
                        && requested.mode == request.mode)
            }) || (request.mode == NonCuratedCacheRefreshMode::IfVersionChanged
                && !state.in_flight
                && state.last_refreshed.as_ref().is_some_and(|last_refreshed| {
                    last_refreshed.roots == request.roots
                        && last_refreshed.configured_plugin_keys == request.configured_plugin_keys
                        && last_refreshed.configured_plugin_sources
                            == request.configured_plugin_sources
                        && (last_refreshed.git_mode == request.git_mode
                            || last_refreshed.git_mode == PluginGitMode::Manual)
                }))
            {
                return;
            }
            state.requested = Some(request);
            if state.in_flight {
                false
            } else {
                state.in_flight = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let manager = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("plugins-non-curated-cache-refresh".to_string())
            .spawn(move || manager.run_non_curated_plugin_cache_refresh_loop())
        {
            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            state.in_flight = false;
            state.requested = None;
            self.non_curated_cache_refresh_completion
                .send_modify(|completion| {
                    completion.sequence = completion.sequence.wrapping_add(1);
                });
            warn!("failed to start non-curated plugin cache refresh task: {err}");
        }
    }

    fn start_curated_repo_sync(
        self: &Arc<Self>,
        http_client_factory: HttpClientFactory,
        on_effective_plugins_changed: Option<EffectivePluginsChangedCallback>,
    ) {
        if CURATED_REPO_SYNC_STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let on_effective_plugins_changed =
            on_effective_plugins_changed.map(|on_effective_plugins_changed| {
                let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                    return on_effective_plugins_changed;
                };
                let callback: EffectivePluginsChangedCallback = Arc::new(move |change| {
                    let on_effective_plugins_changed = Arc::clone(&on_effective_plugins_changed);
                    runtime.spawn(async move {
                        on_effective_plugins_changed(change);
                    });
                });
                callback
            });
        let manager = Arc::clone(self);
        let codex_home = self.codex_home.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("plugins-curated-repo-sync".to_string())
            .spawn(move || {
                match sync_openai_plugins_repo(codex_home.as_path(), http_client_factory) {
                    Ok(curated_plugin_version) => {
                        let configured_curated_plugin_ids =
                            configured_curated_plugin_ids_from_codex_home(codex_home.as_path());
                        match refresh_curated_plugin_cache(
                            codex_home.as_path(),
                            &curated_plugin_version,
                            &configured_curated_plugin_ids,
                        ) {
                            Ok(cache_refreshed) => {
                                manager.clear_caches_after_marketplace_source_refresh(
                                    cache_refreshed,
                                    on_effective_plugins_changed.as_ref(),
                                );
                            }
                            Err(err) => {
                                manager.clear_cache();
                                CURATED_REPO_SYNC_STARTED.store(false, Ordering::SeqCst);
                                warn!("failed to refresh curated plugin cache after sync: {err}");
                            }
                        }
                    }
                    Err(err) => {
                        CURATED_REPO_SYNC_STARTED.store(false, Ordering::SeqCst);
                        warn!("failed to sync curated plugins repo: {err}");
                    }
                }
            })
        {
            CURATED_REPO_SYNC_STARTED.store(false, Ordering::SeqCst);
            warn!("failed to start curated plugins repo sync task: {err}");
        }
    }

    async fn run_remote_installed_plugins_cache_refresh_loop(
        self: Arc<Self>,
        cache_identity: PluginAuthCacheIdentity,
    ) {
        loop {
            let request = {
                let mut states = match self.remote_installed_plugins_cache_refresh_state.write() {
                    Ok(states) => states,
                    Err(err) => err.into_inner(),
                };
                let state = states.entry(cache_identity.clone()).or_default();
                match state.requested.take() {
                    Some(request) => request,
                    None => {
                        state.in_flight = false;
                        return;
                    }
                }
            };

            let plugins = crate::remote::fetch_remote_installed_plugins(
                &request.service_config,
                request.auth_job.auth(),
            )
            .await;
            match plugins {
                Ok(plugins) => {
                    let Some(changed) = self.write_remote_installed_plugins_cache_snapshot(
                        &request.cache_identity,
                        request.generation,
                        plugins,
                        request.auth_job.auth(),
                        &request.service_config.chatgpt_base_url,
                        RemoteInstalledPluginsCachePublication::Refresh,
                    ) else {
                        continue;
                    };
                    let should_notify = changed
                        || !request.change.materialized_remote_plugins.is_empty()
                        || matches!(
                            request.notify,
                            RemoteInstalledPluginsCacheRefreshNotify::AfterSuccessfulRefresh
                        );
                    if should_notify
                        && let Some(on_effective_plugins_changed) =
                            request.on_effective_plugins_changed
                    {
                        on_effective_plugins_changed(request.change);
                    }
                }
                Err(
                    RemotePluginCatalogError::AuthRequired
                    | RemotePluginCatalogError::UnsupportedAuthMode,
                ) => {
                    let Some(changed) = self.clear_remote_installed_plugins_cache_if_current(
                        &request.cache_identity,
                        request.generation,
                    ) else {
                        continue;
                    };
                    if changed
                        && let Some(on_effective_plugins_changed) =
                            request.on_effective_plugins_changed
                    {
                        on_effective_plugins_changed(EffectivePluginsChange::default());
                    }
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        materialized_remote_plugin_count = request
                            .change
                            .materialized_remote_plugins
                            .len(),
                        "failed to refresh remote installed plugins cache"
                    );
                }
            }
        }
    }

    async fn run_remote_installed_plugin_bundle_sync_loop(
        self: Arc<Self>,
        cache_identity: PluginAuthCacheIdentity,
        _registration: RemoteInstalledPluginBundleSyncRegistration,
    ) {
        loop {
            let request = {
                let mut states = match self.remote_installed_plugin_bundle_sync_state.write() {
                    Ok(states) => states,
                    Err(err) => err.into_inner(),
                };
                let state = states.entry(cache_identity.clone()).or_default();
                match state.requested.take() {
                    Some(request) => {
                        state.active_service_config =
                            Some(remote_plugin_service_config(&request.config));
                        state.active_has_callback = request.on_effective_plugins_changed.is_some();
                        request
                    }
                    None => {
                        state.in_flight = false;
                        state.active_service_config = None;
                        state.active_has_callback = false;
                        return;
                    }
                }
            };

            let Ok(_permit) = Arc::clone(&self.remote_installed_plugin_bundle_sync_gate)
                .acquire_owned()
                .await
            else {
                let mut states = self
                    .remote_installed_plugin_bundle_sync_state
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(state) = states.get_mut(&cache_identity) {
                    state.in_flight = false;
                    state.active_service_config = None;
                    state.active_has_callback = false;
                }
                return;
            };
            let result = crate::remote::sync_remote_installed_plugin_bundles_once(
                self.codex_home.clone(),
                &remote_plugin_service_config(&request.config),
                request.auth_job.auth(),
            )
            .await;
            match result {
                Ok(outcome) => {
                    tracing::info!(
                        materialized_remote_plugins = ?outcome.materialized_remote_plugins,
                        changed_plugins = ?outcome.changed_plugins,
                        failed_remote_plugin_ids = ?outcome.failed_remote_plugin_ids,
                        "completed remote installed plugin bundle sync"
                    );
                    if outcome.changed_local_cache() {
                        self.maybe_start_remote_installed_plugins_cache_refresh_with_notify(
                            &request.config,
                            request.auth_job,
                            RemoteInstalledPluginsCacheRefreshNotify::AfterSuccessfulRefresh,
                            request.on_effective_plugins_changed,
                            EffectivePluginsChange {
                                materialized_remote_plugins: outcome.materialized_remote_plugins,
                            },
                        );
                    }
                }
                Err(err) => {
                    warn!(error = %err, "remote installed plugin bundle sync failed");
                }
            }
        }
    }

    async fn run_remote_catalog_cache_refresh_loop(
        self: Arc<Self>,
        cache_identity: PluginAuthCacheIdentity,
    ) {
        loop {
            let request = {
                let mut states = match self.remote_catalog_cache_refresh_state.write() {
                    Ok(states) => states,
                    Err(err) => err.into_inner(),
                };
                let state = states.entry(cache_identity.clone()).or_default();
                match state.requests.pop_front() {
                    Some(request) => request,
                    None => {
                        state.in_flight = false;
                        return;
                    }
                }
            };

            for scope in request.scopes {
                if request.mode == RemoteCatalogCacheRefreshMode::OnlyIfStale
                    && crate::remote::has_fresh_cached_remote_plugin_catalog(
                        self.codex_home.as_path(),
                        &request.service_config,
                        request.auth_job.auth(),
                        scope,
                    )
                {
                    continue;
                }

                match crate::remote::fetch_and_cache_remote_plugin_catalog(
                    self.codex_home.as_path(),
                    &request.service_config,
                    request.auth_job.auth(),
                    scope,
                )
                .await
                {
                    Ok(()) => {}
                    Err(
                        RemotePluginCatalogError::AuthRequired
                        | RemotePluginCatalogError::UnsupportedAuthMode,
                    ) => {}
                    Err(err) => {
                        warn!(
                            error = %err,
                            scope = ?scope,
                            "failed to refresh cached remote plugin catalog"
                        );
                    }
                }
            }
        }
    }

    fn run_non_curated_plugin_cache_refresh_loop(self: Arc<Self>) {
        loop {
            let request = {
                let state = match self.non_curated_cache_refresh_state.read() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                state.requested.clone()
            };

            let Some(request) = request else {
                let mut state = match self.non_curated_cache_refresh_state.write() {
                    Ok(state) => state,
                    Err(err) => err.into_inner(),
                };
                state.in_flight = false;
                self.non_curated_cache_refresh_completion
                    .send_modify(|completion| {
                        completion.sequence = completion.sequence.wrapping_add(1);
                    });
                return;
            };

            let refresh_result = match request.mode {
                NonCuratedCacheRefreshMode::IfVersionChanged => {
                    refresh_non_curated_plugin_cache_detailed(
                        self.codex_home.as_path(),
                        &request.roots,
                        &request.configured_plugin_keys,
                        request.git_mode,
                    )
                }
                NonCuratedCacheRefreshMode::ForceReinstall => {
                    refresh_non_curated_plugin_cache_force_reinstall_detailed(
                        self.codex_home.as_path(),
                        &request.roots,
                        &request.configured_plugin_keys,
                        request.git_mode,
                    )
                }
            };
            let (refreshed, cache_changed) = match refresh_result {
                Ok(refresh_outcome) => {
                    if refresh_outcome.cache_refreshed {
                        self.clear_cache();
                    }
                    for error in &refresh_outcome.errors {
                        warn!(
                            marketplace = error.marketplace_name,
                            error = %error.message,
                            "failed to refresh configured plugin cache"
                        );
                    }
                    (
                        refresh_outcome.errors.is_empty(),
                        refresh_outcome.cache_refreshed,
                    )
                }
                Err(err) => {
                    self.clear_cache();
                    warn!("failed to refresh non-curated plugin cache: {err}");
                    (false, false)
                }
            };

            let mut state = match self.non_curated_cache_refresh_state.write() {
                Ok(state) => state,
                Err(err) => err.into_inner(),
            };
            if refreshed {
                state.last_refreshed = Some(request.clone());
            }
            let complete = state.requested.as_ref() == Some(&request);
            if complete {
                state.requested = None;
                state.in_flight = false;
            }
            self.non_curated_cache_refresh_completion
                .send_modify(|completion| {
                    completion.sequence = completion.sequence.wrapping_add(1);
                    if cache_changed {
                        completion.changed_sequence = completion.changed_sequence.wrapping_add(1);
                    }
                });
            if complete {
                return;
            }
        }
    }

    fn configured_plugin_states(&self, config: &PluginsConfigInput) -> ConfiguredPluginStates {
        let configured_plugins =
            configured_plugins_from_stack(&config.config_layer_stack, self.codex_home.as_path());
        let installed = configured_plugins
            .keys()
            .filter(|plugin_key| {
                PluginId::parse(plugin_key)
                    .ok()
                    .is_some_and(|plugin_id| self.store.is_installed(&plugin_id))
            })
            .cloned()
            .collect::<HashSet<_>>();
        let enabled = configured_plugins
            .into_iter()
            .filter_map(|(plugin_key, plugin)| plugin.enabled.then_some(plugin_key))
            .collect::<HashSet<_>>();
        ConfiguredPluginStates { installed, enabled }
    }

    fn marketplace_roots_with_auth(
        &self,
        config: &PluginsConfigInput,
        additional_roots: &[AbsolutePathBuf],
        include_openai_curated: bool,
        auth: Option<&CodexAuth>,
    ) -> Vec<AbsolutePathBuf> {
        // Treat the curated catalog as an extra marketplace root so plugin listing can surface it
        // without requiring every caller to know where it is stored.
        let mut roots = additional_roots.to_vec();
        roots.extend(installed_marketplace_roots_from_layer_stack(
            &config.config_layer_stack,
            self.codex_home.as_path(),
        ));
        let curated_marketplace_path = if include_openai_curated {
            match target_curated_marketplace(projected_auth_mode(auth)) {
                TargetCuratedMarketplace::OpenAi | TargetCuratedMarketplace::OpenAiWithRemote => {
                    let curated_repo_root = curated_plugins_repo_path(self.codex_home.as_path());
                    curated_repo_root.is_dir().then_some(curated_repo_root)
                }
                TargetCuratedMarketplace::OpenAiApi => {
                    let api_marketplace_path =
                        curated_plugins_api_marketplace_path(self.codex_home.as_path());
                    api_marketplace_path
                        .is_file()
                        .then_some(api_marketplace_path)
                }
            }
        } else {
            None
        };
        if let Some(curated_marketplace_path) = curated_marketplace_path
            && let Ok(curated_marketplace_path) =
                AbsolutePathBuf::try_from(curated_marketplace_path)
        {
            roots.push(curated_marketplace_path);
        }
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    fn list_marketplaces_with_policy(
        &self,
        config: &PluginsConfigInput,
        roots: &[AbsolutePathBuf],
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        let mut outcome = list_marketplaces_with_home(roots, home_dir().as_deref())?;
        let policy = MarketplacePolicy::from_requirements(config.config_layer_stack.requirements());
        outcome.marketplaces.retain(|marketplace| {
            policy
                .validate_install(
                    &config.config_layer_stack,
                    self.codex_home.as_path(),
                    &marketplace.path,
                    &marketplace.name,
                )
                .is_ok()
        });
        Ok(outcome)
    }
}

pub(crate) fn remote_plugin_install_required_description(
    source: &MarketplacePluginSource,
) -> String {
    let source_description = match source {
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => {
            let mut parts = vec![url.clone()];
            if let Some(path) = path {
                parts.push(format!("path `{path}`"));
            }
            if let Some(ref_name) = ref_name {
                parts.push(format!("ref `{ref_name}`"));
            }
            if let Some(sha) = sha {
                parts.push(format!("sha `{sha}`"));
            }
            parts.join(", ")
        }
        MarketplacePluginSource::Local { path } => path.as_path().display().to_string(),
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => {
            let mut parts = vec![package.clone()];
            if let Some(version) = version {
                parts.push(format!("version `{version}`"));
            }
            if let Some(registry) = registry {
                parts.push(format!("registry `{registry}`"));
            }
            parts.join(", ")
        }
    };

    let source_kind = if matches!(source, MarketplacePluginSource::Npm { .. }) {
        "an npm plugin"
    } else {
        "a cross-repo plugin"
    };
    format!(
        "This is {source_kind}. Install it to view more detailed information. The source of the plugin is {source_description}."
    )
}

#[derive(Debug, thiserror::Error)]
pub enum PluginInstallError {
    #[error("{0}")]
    Marketplace(#[from] MarketplaceError),

    #[error("{0}")]
    Remote(#[from] RemotePluginMutationError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("{0}")]
    Config(#[from] anyhow::Error),

    #[error("failed to join plugin install task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl PluginInstallError {
    fn join(source: tokio::task::JoinError) -> Self {
        Self::Join(source)
    }

    pub fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            Self::Marketplace(
                MarketplaceError::MarketplaceNotFound { .. }
                    | MarketplaceError::InvalidMarketplaceFile { .. }
                    | MarketplaceError::PluginNotFound { .. }
                    | MarketplaceError::PluginNotAvailable { .. }
                    | MarketplaceError::InvalidPlugin(_)
            ) | Self::Store(PluginStoreError::Invalid(_))
        )
    }

    pub fn sub_error_type(&self) -> Option<String> {
        match self {
            Self::Marketplace(err) => marketplace_error_sub_error_type(err),
            Self::Remote(err) => err.sub_error_type(),
            Self::Store(err) => err.sub_error_type(),
            Self::Config(_) => Some("failed_to_enable_plugin".to_string()),
            Self::Join(_) => Some("plugin_install_task_failed".to_string()),
        }
    }
}

fn plugin_install_error_type(err: &PluginInstallError) -> &'static str {
    match err {
        PluginInstallError::Marketplace(err) => marketplace_error_type(err),
        PluginInstallError::Remote(err) => remote_plugin_mutation_error_type(err),
        PluginInstallError::Store(err) => plugin_store_error_type(err),
        PluginInstallError::Config(_) => "config",
        PluginInstallError::Join(_) => "join",
    }
}

fn marketplace_error_type(err: &MarketplaceError) -> &'static str {
    match err {
        MarketplaceError::Io { .. } => "marketplace_io",
        MarketplaceError::MarketplaceNotFound { .. } => "marketplace_not_found",
        MarketplaceError::InvalidMarketplaceFile { .. } => "invalid_marketplace_file",
        MarketplaceError::PluginNotFound { .. } => "plugin_not_found",
        MarketplaceError::PluginNotAvailable { .. } => "plugin_not_available",
        MarketplaceError::PluginsDisabled => "plugins_disabled",
        MarketplaceError::InvalidPlugin(_) => "invalid_plugin",
    }
}

fn marketplace_error_sub_error_type(err: &MarketplaceError) -> Option<String> {
    match err {
        MarketplaceError::Io { context, .. } => Some(error_context_sub_error_type(context)),
        MarketplaceError::MarketplaceNotFound { .. }
        | MarketplaceError::InvalidMarketplaceFile { .. }
        | MarketplaceError::PluginNotFound { .. }
        | MarketplaceError::PluginNotAvailable { .. }
        | MarketplaceError::PluginsDisabled
        | MarketplaceError::InvalidPlugin(_) => None,
    }
}

fn remote_plugin_mutation_error_type(err: &RemotePluginMutationError) -> &'static str {
    match err {
        RemotePluginMutationError::AuthRequired => "remote_mutation_auth_required",
        RemotePluginMutationError::UnsupportedAuthMode => "remote_mutation_unsupported_auth_mode",
        RemotePluginMutationError::AuthToken(_) => "remote_mutation_auth_token",
        RemotePluginMutationError::InvalidBaseUrl(_) => "remote_mutation_invalid_base_url",
        RemotePluginMutationError::InvalidBaseUrlPath => "remote_mutation_invalid_base_url_path",
        RemotePluginMutationError::Request { .. } => "remote_mutation_request",
        RemotePluginMutationError::UnexpectedStatus { .. } => "remote_mutation_unexpected_status",
        RemotePluginMutationError::Decode { .. } => "remote_mutation_decode",
        RemotePluginMutationError::UnexpectedPluginId { .. } => {
            "remote_mutation_unexpected_plugin_id"
        }
        RemotePluginMutationError::UnexpectedEnabledState { .. } => {
            "remote_mutation_unexpected_enabled_state"
        }
    }
}

fn plugin_store_error_type(err: &PluginStoreError) -> &'static str {
    match err {
        PluginStoreError::Io { .. } => "store_io",
        PluginStoreError::Invalid(_) => "store_invalid",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginUninstallError {
    #[error("{0}")]
    InvalidPluginId(#[from] PluginIdError),

    #[error("{0}")]
    Remote(#[from] RemotePluginMutationError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("{0}")]
    Config(#[from] anyhow::Error),

    #[error("failed to join plugin uninstall task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl PluginUninstallError {
    fn join(source: tokio::task::JoinError) -> Self {
        Self::Join(source)
    }

    pub fn is_invalid_request(&self) -> bool {
        matches!(self, Self::InvalidPluginId(_))
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
