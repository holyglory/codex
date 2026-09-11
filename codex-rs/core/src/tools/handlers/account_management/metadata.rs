use codex_login::ManagedAccountUpdate;
use serde::Serialize;

use super::AccountOutput;
use super::account_output;
use super::encode_output;
use super::routed_account_alias;
use super::tool_error;
use super::validate_reference;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetadataMutationOutput {
    generation: u64,
    changed: bool,
    default_account: Option<String>,
    routed_account: Option<String>,
    auto_selection_enabled: bool,
    account: Option<AccountOutput>,
}

pub(super) fn update(
    invocation: &ToolInvocation,
    update: ManagedAccountUpdate,
    expected_generation: u64,
) -> Result<String, FunctionCallError> {
    match &update {
        ManagedAccountUpdate::Rename { account, .. }
        | ManagedAccountUpdate::Enable { account }
        | ManagedAccountUpdate::Disable { account }
        | ManagedAccountUpdate::SetDefault { account } => validate_reference(account)?,
        ManagedAccountUpdate::EnableAutomaticSelection
        | ManagedAccountUpdate::DisableAutomaticSelection => {}
    }
    let result = codex_login::update_managed_account(
        &invocation.turn.config.auth_config(),
        update,
        expected_generation,
    )
    .map_err(tool_error)?;
    let routed_account = routed_account_alias(invocation, &result.snapshot);
    let account = result
        .account_id
        .as_ref()
        .and_then(|id| {
            result
                .snapshot
                .accounts
                .iter()
                .find(|account| &account.account_id == id)
        })
        .map(|account| account_output(account, routed_account.as_deref()));
    encode_output(&MetadataMutationOutput {
        generation: result.snapshot.generation,
        changed: result.changed,
        default_account: result
            .snapshot
            .accounts
            .iter()
            .find(|account| account.is_default)
            .map(|account| account.alias.clone()),
        routed_account,
        auto_selection_enabled: result.snapshot.auto_selection_enabled,
        account,
    })
}
