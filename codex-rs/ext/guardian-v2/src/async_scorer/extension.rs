//! Registers Guardian classification and handles thread, skill, and tool lifecycle hooks.

use std::sync::Arc;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::context::GuardianReviewEvidence;
use codex_core::context::NodeReplReviewEvidence;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::GuardianV2Enabled;
use codex_extension_api::SkillInvocationContributor;
use codex_extension_api::SkillInvocationInput;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_login::AuthManager;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::openai_models::ModelInfo;

use super::config::GuardianV2Config;
use super::sampler::LunaSampler;
use super::score::GuardianV2ScoreProgress;
#[path = "auth_lifecycle.rs"]
mod auth_lifecycle;
use super::trusted_skills::TrustedSkillRoots;

#[derive(Clone)]
pub(super) struct GuardianV2Extension {
    auth_manager: Arc<AuthManager>,
    pub(super) auth_resolver: Option<codex_login::SharedProfileAuthRouter>,
    pub(super) event_sink: Arc<dyn ExtensionEventSink>,
    pub(super) thread_manager: Weak<ThreadManager>,
}

impl SkillInvocationContributor for GuardianV2Extension {
    fn requires_host_skill_discovery(&self) -> bool {
        false
    }

    fn on_skill_invocation<'a>(
        &'a self,
        input: SkillInvocationInput<'a>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(roots) = input.thread_store.get::<TrustedSkillRoots>() else {
                return;
            };
            let Some(skill_path) = roots.trusted_skill_path(input.skill_resource) else {
                return;
            };
            let Some(evidence) = input.thread_store.get::<GuardianReviewEvidence>() else {
                return;
            };
            evidence.record_trusted_skill(input.turn_id, skill_path);
        })
    }
}

impl ToolLifecycleContributor for GuardianV2Extension {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(self.score_tool(input))
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if let Some(progress) = input.thread_store.get::<GuardianV2ScoreProgress>() {
                progress.finish(input.call_id);
            }
        })
    }
}

/// Installs feature-gated Guardian V2 tool classification for each thread.
pub fn install(
    registry: &mut ExtensionRegistryBuilder<Config>,
    auth_manager: Arc<AuthManager>,
    thread_manager: Weak<ThreadManager>,
) {
    install_inner(
        registry,
        auth_manager,
        /*auth_resolver*/ None,
        thread_manager,
    );
}

pub fn install_with_auth_resolver(
    registry: &mut ExtensionRegistryBuilder<Config>,
    auth_manager: Arc<AuthManager>,
    auth_resolver: codex_login::SharedProfileAuthRouter,
    thread_manager: Weak<ThreadManager>,
) {
    install_inner(registry, auth_manager, Some(auth_resolver), thread_manager);
}

fn install_inner(
    registry: &mut ExtensionRegistryBuilder<Config>,
    auth_manager: Arc<AuthManager>,
    auth_resolver: Option<codex_login::SharedProfileAuthRouter>,
    thread_manager: Weak<ThreadManager>,
) {
    let uses_turn_auth = auth_resolver.is_some();
    let extension = Arc::new(GuardianV2Extension {
        auth_manager,
        auth_resolver,
        event_sink: registry.event_sink(),
        thread_manager,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.approval_review_contributor(Arc::new(super::approval::GuardianApprovalReviewer {
        thread_manager: extension.thread_manager.clone(),
    }));
    if uses_turn_auth {
        registry.turn_lifecycle_contributor(extension.clone());
    }
    registry.skill_invocation_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension);
}

#[cfg(test)]
#[path = "extension_tests.rs"]
mod tests;
