use std::collections::BTreeMap;

use codex_login::AccountManagementError;
use codex_login::ManagedAccountPriorityMutation;
use codex_login::ManagedAccountSnapshot;
use codex_login::ManagedAccountSummary;
use codex_login::read_managed_accounts;
use codex_login::set_all_managed_account_priorities;
use codex_login::set_managed_account_priority;
use codex_protocol::auth::AuthMode;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

use self::service_usage::ServiceUsageOutput;
use self::service_usage::add_service_usage;

mod service_usage;

const DEFAULT_PAGE_LIMIT: u32 = 10;
const MAX_PAGE_LIMIT: u32 = 25;
const MAX_USAGE_PAGE_LIMIT: u32 = 10;
const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

pub struct AccountManagementHandler;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum AccountManagementArgs {
    List {
        #[serde(default)]
        offset: u32,
        limit: Option<u32>,
        #[serde(default)]
        refresh_service_usage: bool,
    },
    SetPriority {
        account: String,
        priority: u32,
        expected_generation: Option<u64>,
    },
    SetAllPriorities {
        priority: u32,
        expected_generation: Option<u64>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountListOutput {
    generation: u64,
    auto_selection_enabled: bool,
    priority_order: &'static str,
    routed_account: Option<String>,
    accounts: Vec<AccountOutput>,
    next_offset: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_usage_bucket_fields: Option<&'static [&'static str]>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountOutput {
    #[serde(skip)]
    account_id: String,
    alias: String,
    auth_mode: &'static str,
    enabled: bool,
    authenticated: bool,
    priority: u32,
    is_default: bool,
    is_current_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_usage: Option<ServiceUsageOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PriorityMutationOutput {
    generation: u64,
    priority_order: &'static str,
    changed_count: usize,
    priority: u32,
    accounts: Vec<String>,
    accounts_truncated: bool,
}

impl ToolExecutor<ToolInvocation> for AccountManagementHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("account_management")
    }

    fn spec(&self) -> ToolSpec {
        account_management_spec()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "account_management requires a function payload".to_string(),
                ));
            };
            let args: AccountManagementArgs = serde_json::from_str(arguments).map_err(|_| {
                FunctionCallError::RespondToModel(
                    "account_management arguments must match its action schema".to_string(),
                )
            })?;
            let auth_config = invocation.turn.config.auth_config();
            let value = match args {
                AccountManagementArgs::List {
                    offset,
                    limit,
                    refresh_service_usage,
                } => {
                    let limit = validated_limit(limit, refresh_service_usage)?;
                    let snapshot = read_managed_accounts(&auth_config).map_err(tool_error)?;
                    let routed_account = routed_account_alias(&invocation, &snapshot);
                    let page = account_page(snapshot, offset, limit, routed_account)?;
                    let output = if refresh_service_usage {
                        add_service_usage(&invocation, page).await
                    } else {
                        page
                    };
                    encode_output(&output)
                }
                AccountManagementArgs::SetPriority {
                    account,
                    priority,
                    expected_generation,
                } => {
                    validate_reference(&account)?;
                    let mutation = set_managed_account_priority(
                        &auth_config,
                        &account,
                        priority,
                        expected_generation,
                    )
                    .map_err(tool_error)?;
                    encode_output(&single_priority_output(mutation, &account, priority))
                }
                AccountManagementArgs::SetAllPriorities {
                    priority,
                    expected_generation,
                } => {
                    let mutation = set_all_managed_account_priorities(
                        &auth_config,
                        priority,
                        expected_generation,
                    )
                    .map_err(tool_error)?;
                    encode_output(&all_priorities_output(mutation, priority))
                }
            }?;
            Ok(
                boxed_tool_output(FunctionToolOutput::from_text(value, Some(true)))
                    as Box<dyn ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for AccountManagementHandler {}

fn account_page(
    snapshot: ManagedAccountSnapshot,
    offset: u32,
    limit: u32,
    routed_account: Option<String>,
) -> Result<AccountListOutput, FunctionCallError> {
    let offset = usize::try_from(offset).map_err(|_| invalid_pagination())?;
    if offset > snapshot.accounts.len() {
        return Err(invalid_pagination());
    }
    let end = offset
        .saturating_add(limit as usize)
        .min(snapshot.accounts.len());
    let accounts = snapshot.accounts[offset..end]
        .iter()
        .map(|account| account_output(account, routed_account.as_deref()))
        .collect();
    let next_offset = (end < snapshot.accounts.len()).then_some(end as u32);
    Ok(AccountListOutput {
        generation: snapshot.generation,
        auto_selection_enabled: snapshot.auto_selection_enabled,
        priority_order: "higherFirst",
        routed_account,
        accounts,
        next_offset,
        service_usage_bucket_fields: None,
    })
}

fn account_output(account: &ManagedAccountSummary, routed_account: Option<&str>) -> AccountOutput {
    AccountOutput {
        account_id: account.account_id.clone(),
        alias: account.alias.clone(),
        auth_mode: auth_mode_label(account.auth_mode),
        enabled: account.enabled,
        authenticated: account.authenticated,
        priority: account.priority,
        is_default: account.is_default,
        is_current_turn: routed_account == Some(account.alias.as_str()),
        service_usage: None,
    }
}

fn routed_account_alias(
    invocation: &ToolInvocation,
    snapshot: &ManagedAccountSnapshot,
) -> Option<String> {
    let routed_id = invocation
        .turn
        .account_lease
        .as_ref()?
        .account_id()
        .as_str();
    snapshot
        .accounts
        .iter()
        .find(|account| account.account_id == routed_id)
        .map(|account| account.alias.clone())
}

fn single_priority_output(
    mutation: ManagedAccountPriorityMutation,
    reference: &str,
    priority: u32,
) -> PriorityMutationOutput {
    let account = mutation
        .snapshot
        .accounts
        .iter()
        .find(|account| account.alias == reference || account.account_id == reference)
        .map(|account| account.alias.clone())
        .into_iter()
        .collect();
    PriorityMutationOutput {
        generation: mutation.snapshot.generation,
        priority_order: "higherFirst",
        changed_count: mutation.changed_count,
        priority,
        accounts: account,
        accounts_truncated: false,
    }
}

fn all_priorities_output(
    mutation: ManagedAccountPriorityMutation,
    priority: u32,
) -> PriorityMutationOutput {
    let accounts_truncated = mutation.snapshot.accounts.len() > MAX_PAGE_LIMIT as usize;
    let accounts = mutation
        .snapshot
        .accounts
        .iter()
        .take(MAX_PAGE_LIMIT as usize)
        .map(|account| account.alias.clone())
        .collect();
    PriorityMutationOutput {
        generation: mutation.snapshot.generation,
        priority_order: "higherFirst",
        changed_count: mutation.changed_count,
        priority,
        accounts,
        accounts_truncated,
    }
}

fn validated_limit(
    limit: Option<u32>,
    refresh_service_usage: bool,
) -> Result<u32, FunctionCallError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    let maximum = if refresh_service_usage {
        MAX_USAGE_PAGE_LIMIT
    } else {
        MAX_PAGE_LIMIT
    };
    (1..=maximum)
        .contains(&limit)
        .then_some(limit)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "account_management limit must be between 1 and {maximum}"
            ))
        })
}

fn validate_reference(reference: &str) -> Result<(), FunctionCallError> {
    if reference.is_empty() || reference.len() > 64 || reference.chars().any(char::is_control) {
        return Err(FunctionCallError::RespondToModel(
            "account_management account reference is invalid".to_string(),
        ));
    }
    Ok(())
}

fn invalid_pagination() -> FunctionCallError {
    FunctionCallError::RespondToModel("account_management pagination offset is invalid".to_string())
}

fn tool_error(error: AccountManagementError) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

fn encode_output(value: &impl Serialize) -> Result<String, FunctionCallError> {
    let encoded = serde_json::to_string(value).map_err(|_| {
        FunctionCallError::RespondToModel(
            "account_management could not encode its credential-free result".to_string(),
        )
    })?;
    if encoded.len() > MAX_TOOL_OUTPUT_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "account_management result exceeds its {MAX_TOOL_OUTPUT_BYTES}-byte safety limit"
        )));
    }
    Ok(encoded)
}

fn auth_mode_label(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api-key",
        AuthMode::Chatgpt => "chatgpt",
        AuthMode::ChatgptAuthTokens => "external-chatgpt",
        AuthMode::Headers => "headers",
        AuthMode::AgentIdentity => "agent-identity",
        AuthMode::PersonalAccessToken => "personal-access-token",
        AuthMode::BedrockApiKey => "bedrock-api-key",
        AuthMode::BedrockAccessKeys => "bedrock-access-keys",
    }
}

fn account_management_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("list"),
                    json!("set_priority"),
                    json!("set_all_priorities"),
                ],
                Some("Read accounts or change their numeric priority.".to_string()),
            ),
        ),
        (
            "account".to_string(),
            JsonSchema::string(Some(
                "Account alias or local profile ID; required for set_priority.".to_string(),
            )),
        ),
        (
            "priority".to_string(),
            JsonSchema::integer(Some(
                "Unsigned priority; higher numbers drain first and smaller numbers drain last."
                    .to_string(),
            )),
        ),
        (
            "expected_generation".to_string(),
            JsonSchema::integer(Some(
                "Optional registry generation precondition for a mutation.".to_string(),
            )),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some("List offset; defaults to 0.".to_string())),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some(
                "List page size; maximum 25, or 10 with service usage refresh.".to_string(),
            )),
        ),
        (
            "refresh_service_usage".to_string(),
            JsonSchema::boolean(Some(
                "For list only, fetch fresh bounded rate-limit usage for eligible managed ChatGPT profiles."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "account_management".to_string(),
        description: "Read credential-free local account metadata, identify the exact profile routed for this turn, and atomically manage automatic-selection priorities. Use list before mutations; pass expected_generation to prevent stale edits. This tool never returns email, credentials, service/workspace identifiers, or account notes, and it cannot add, authorize, rename, activate, or remove profiles. Use refresh_service_usage only when current service rate-limit data is needed."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "account_management_tests.rs"]
mod tests;
