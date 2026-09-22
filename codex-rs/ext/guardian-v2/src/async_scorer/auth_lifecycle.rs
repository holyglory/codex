use super::*;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadOriginator;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_features::Feature;
use codex_login::AgentIdentityAuthPolicy;
use codex_model_provider::create_model_provider;
use codex_protocol::protocol::has_full_access;

use super::super::sampler::LunaSamplerConfig;
use super::super::sampler::MODEL;

#[derive(Clone)]
struct GuardianSamplerTemplate {
    config: Config,
    session_source: codex_protocol::protocol::SessionSource,
    session_id: String,
    thread_id: String,
    originator: Option<String>,
    luna_compaction_hash: Option<String>,
    metrics: Option<Arc<dyn ExtensionMetrics>>,
    prewarm_allowed: bool,
    scoring_enabled: bool,
    max_input_tokens: usize,
}

impl ThreadLifecycleContributor<Config> for GuardianV2Extension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !input.config.features.enabled(Feature::GuardianV2)
                || !input.config.features.enabled(Feature::GuardianApproval)
            {
                return;
            }

            let thread_id = input.thread_store.level_id().to_string();
            let guardian_config = match GuardianV2Config::resolve(input.config) {
                Ok(config) => config,
                Err(error) => {
                    self.event_sink.emit_warning(ExtensionWarning {
                        thread_id,
                        turn_id: None,
                        message: error,
                    });
                    return;
                }
            };
            let model = input.thread_store.get::<ModelInfo>();
            let mut policy = guardian_config.policy_for_model(model.as_deref());
            if let Some(model) = &model {
                input
                    .config
                    .config_layer_stack
                    .requirements()
                    .constrain_guardian_policy(&mut policy, &model.slug);
            }
            let scoring_enabled = policy.scoring_enabled();
            // Keep the upstream background prewarm when this process still uses the singular
            // authentication path. A configured profile registry must defer construction until
            // the owning turn supplies its exact account lease; router errors fail closed here.
            let requires_turn_auth = match &self.auth_resolver {
                Some(auth_resolver) => !matches!(
                    auth_resolver.router_if_configured().await,
                    Ok(/*router*/ None)
                ),
                None => false,
            };
            let standalone_sampler = if requires_turn_auth {
                None
            } else {
                Some(
                    super::super::startup::sampler_config(
                        &input,
                        Arc::clone(&self.auth_manager),
                        self.thread_manager.upgrade(),
                    )
                    .await,
                )
            };
            let luna_compaction_hash = standalone_sampler
                .as_ref()
                .and_then(|config| config.luna_compaction_hash.clone());
            if scoring_enabled && guardian_config.transcript.include_images {
                input
                    .thread_store
                    .get_or_init(NodeReplReviewEvidence::default)
                    .enable_image_capture();
            }
            let prewarm_allowed = scoring_enabled
                && input.config.approvals_reviewer == ApprovalsReviewer::AutoReview
                && !has_full_access(
                    input.config.permissions.approval_policy.value(),
                    &input.config.permissions.effective_permission_profile(),
                    input
                        .environments
                        .iter()
                        .map(|environment| &environment.config),
                );
            let template = GuardianSamplerTemplate {
                config: input.config.clone(),
                session_source: input.session_source.clone(),
                session_id: input.session_store.level_id().to_string(),
                thread_id,
                originator: input
                    .thread_store
                    .get::<ThreadOriginator>()
                    .map(|originator| originator.0.clone()),
                luna_compaction_hash,
                metrics: input.extension_metrics.clone(),
                prewarm_allowed,
                scoring_enabled,
                max_input_tokens: codex_guardian_context::DEFAULT_MAX_INPUT_TOKENS,
            };
            input.thread_store.insert(template.clone());
            input.thread_store.insert(guardian_config);
            input.thread_store.insert(GuardianV2ScoreProgress::new(
                input.extension_metrics.clone(),
            ));
            input
                .thread_store
                .get_or_init(GuardianReviewEvidence::default);
            input
                .thread_store
                .insert(TrustedSkillRoots::from_config(input.config));
            if !requires_turn_auth {
                let _ = input.thread_store.remove::<LunaSampler>();
                let sampler = input.thread_store.get_or_init(|| {
                    LunaSampler::new(standalone_sampler.expect("singular auth sampler"))
                });
                if template.scoring_enabled {
                    input.thread_store.insert(GuardianV2Enabled);
                }
                if template.prewarm_allowed {
                    tokio::spawn(async move {
                        sampler.prewarm().await;
                    });
                }
            }
        })
    }
}

impl TurnLifecycleContributor for GuardianV2Extension {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if self.auth_resolver.is_none() {
                return;
            }
            let Some(template) = input.thread_store.get::<GuardianSamplerTemplate>() else {
                return;
            };
            let Some(auth_lease) = input.turn_store.get::<codex_login::AuthManagerLease>() else {
                self.event_sink.emit_warning(ExtensionWarning {
                    thread_id: template.thread_id.clone(),
                    turn_id: Some(input.turn_id.to_string()),
                    message: "Guardian V2 authentication is unavailable for this turn.".to_string(),
                });
                return;
            };
            if !auth_lease.is_profile_scoped() && input.thread_store.get::<LunaSampler>().is_some()
            {
                return;
            }
            let _ = input.thread_store.remove::<LunaSampler>();
            let _ = input.thread_store.remove::<GuardianV2Enabled>();
            let turn_config = Self::config_for_auth_lease(&template.config, &auth_lease);
            let mut model_config = turn_config.to_models_manager_config();
            model_config.model_context_window = None;
            let luna_model = codex_core::build_models_manager(
                &turn_config,
                Arc::clone(auth_lease.auth_manager()),
            )
            .get_model_info(MODEL, &model_config)
            .await;
            let mut template = (*template).clone();
            template.max_input_tokens = codex_guardian_context::effective_input_token_limit(
                &luna_model,
                /*configured_window*/ None,
            );
            let luna_compaction_hash = luna_model.comp_hash;
            let sampler = input.thread_store.get_or_init(|| {
                Self::create_sampler(
                    &template,
                    &turn_config,
                    Arc::clone(auth_lease.auth_manager()),
                    luna_compaction_hash,
                )
            });
            if template.scoring_enabled {
                input.thread_store.insert(GuardianV2Enabled);
            }
            if template.prewarm_allowed {
                tokio::spawn(async move {
                    sampler.prewarm().await;
                });
            }
        })
    }
}

impl GuardianV2Extension {
    pub(in crate::async_scorer) fn config_for_auth_lease(
        config: &Config,
        auth_lease: &codex_login::AuthManagerLease,
    ) -> Config {
        let mut config = config.clone();
        if let Some(account_id) = auth_lease.account_id() {
            config.codex_home = config.codex_home.join("accounts").join(account_id.as_str());
        }
        config
    }

    fn create_sampler(
        template: &GuardianSamplerTemplate,
        config: &Config,
        auth_manager: Arc<AuthManager>,
        luna_compaction_hash: Option<String>,
    ) -> LunaSampler {
        LunaSampler::new(LunaSamplerConfig {
            provider: create_model_provider(config.model_provider.clone(), Some(auth_manager)),
            http_client_factory: config.http_client_factory(),
            agent_identity_policy: if config.features.enabled(Feature::UseAgentIdentity) {
                AgentIdentityAuthPolicy::ChatGptAuth
            } else {
                AgentIdentityAuthPolicy::JwtOnly
            },
            session_source: template.session_source.clone(),
            session_id: template.session_id.clone(),
            thread_id: template.thread_id.clone(),
            originator: template.originator.clone(),
            max_input_tokens: template.max_input_tokens,
            service_tier: config.service_tier.clone(),
            luna_compaction_hash,
            metrics: template.metrics.clone(),
        })
    }
}
