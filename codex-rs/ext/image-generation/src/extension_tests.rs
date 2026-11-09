use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::SelectedPluginSnapshot;
use codex_extension_api::StepPreparationInput;
use codex_extension_api::ToolContributor;
use codex_login::AuthManager;
use codex_login::AuthManagerLease;
use codex_login::CodexAuth;

use super::ImageGenerationExtension;

#[tokio::test]
async fn image_generation_step_uses_the_turn_account_lease() {
    let process_auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("process"));
    let selected_auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("selected"));
    let extension = ImageGenerationExtension {
        auth_manager: Arc::clone(&process_auth),
        resolve_save_root: Arc::new(|_| None),
    };
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let turn_store = ExtensionData::new("turn");
    turn_store.insert(AuthManagerLease::legacy(Arc::clone(&selected_auth)));
    let step_store = ExtensionData::new("step");
    let selected_plugins = SelectedPluginSnapshot::default();
    let roots = [];

    extension
        .prepare_for_step(StepPreparationInput {
            turn_id: "turn",
            ready_selected_capability_roots: &roots,
            executor_capability_discovery: None,
            executor_capability_discovery_v2: None,
            selected_plugins: &selected_plugins,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: &turn_store,
            step_store: &step_store,
        })
        .await;

    let lease = step_store
        .get::<AuthManagerLease>()
        .expect("turn account lease should be copied into the step");
    assert!(Arc::ptr_eq(lease.auth_manager(), &selected_auth));
    assert!(!Arc::ptr_eq(lease.auth_manager(), &process_auth));
}
