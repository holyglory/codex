use crate::JsonSchema;
use crate::TS;
use crate::protocol::common::AuthMode;
use codex_protocol::account::PlanType;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;

use super::CancelLoginAccountStatus;
use super::LoginAppBrand;
use super::RateLimitSnapshot;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MultiAccountCapability {
    pub version: u32,
    pub supports_managed_login: bool,
    pub supports_auto_selection: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfile {
    pub id: String,
    pub alias: String,
    pub auth_mode: AuthMode,
    pub email: Option<String>,
    pub plan_type: Option<PlanType>,
    pub enabled: bool,
    /// Whether this profile currently has locally managed credentials.
    /// Credential values are never returned.
    pub authenticated: bool,
    pub priority: u32,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub note: Option<String>,
    pub is_default: bool,
    pub is_active: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AccountAutoSelectionPolicy {
    Priority,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum AccountPriorityOrder {
    HigherFirst,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountAutoSelection {
    pub enabled: bool,
    pub policy: AccountAutoSelectionPolicy,
    pub priority_order: AccountPriorityOrder,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileListParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileListResponse {
    pub data: Vec<AccountProfile>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileReadParams {
    pub account_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileReadResponse {
    pub profile: AccountProfile,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileActivateParams {
    pub account_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileActivateResponse {
    pub profile: AccountProfile,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileUpdateParams {
    pub account_id: String,
    #[ts(optional = nullable)]
    pub alias: Option<String>,
    #[ts(optional = nullable)]
    pub enabled: Option<bool>,
    #[ts(optional = nullable)]
    pub priority: Option<u32>,
    #[ts(optional = nullable)]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_note: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileUpdateResponse {
    pub profile: AccountProfile,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileRemoveParams {
    pub account_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileRemoveResponse {
    pub account_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileLoginStartParams {
    #[ts(optional = nullable)]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub activate: bool,
    pub login: AccountProfileLoginMethodParams,
}

impl fmt::Debug for AccountProfileLoginStartParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountProfileLoginStartParams")
            .field("alias", &self.alias)
            .field("activate", &self.activate)
            .field("login", &"<redacted>")
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileLoginStartResponse {
    pub account_id: String,
    pub login: AccountProfileLogin,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum AccountProfileLoginMethodParams {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    #[ts(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {
        #[serde(rename = "apiKey")]
        #[ts(rename = "apiKey")]
        api_key: String,
    },
    #[serde(rename = "chatgpt", rename_all = "camelCase")]
    #[ts(rename = "chatgpt", rename_all = "camelCase")]
    Chatgpt {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        codex_streamlined_login: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        use_hosted_login_success_page: bool,
        #[serde(default)]
        #[ts(optional = nullable)]
        app_brand: Option<LoginAppBrand>,
    },
    #[serde(rename = "chatgptDeviceCode")]
    #[ts(rename = "chatgptDeviceCode")]
    ChatgptDeviceCode,
    #[serde(rename = "amazonBedrock", rename_all = "camelCase")]
    #[ts(rename = "amazonBedrock", rename_all = "camelCase")]
    AmazonBedrock { api_key: String, region: String },
}

impl fmt::Debug for AccountProfileLoginMethodParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ApiKey { .. } => "AccountProfileLoginMethod::ApiKey([redacted])",
            Self::Chatgpt { .. } => "AccountProfileLoginMethod::Chatgpt",
            Self::ChatgptDeviceCode => "AccountProfileLoginMethod::ChatgptDeviceCode",
            Self::AmazonBedrock { .. } => "AccountProfileLoginMethod::AmazonBedrock([redacted])",
        })
    }
}

pub type AccountProfileLoginMethod = AccountProfileLoginMethodParams;

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum AccountProfileLogin {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    #[ts(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {},
    #[serde(rename = "chatgpt", rename_all = "camelCase")]
    #[ts(rename = "chatgpt", rename_all = "camelCase")]
    Chatgpt { login_id: String, auth_url: String },
    #[serde(rename = "chatgptDeviceCode", rename_all = "camelCase")]
    #[ts(rename = "chatgptDeviceCode", rename_all = "camelCase")]
    ChatgptDeviceCode {
        login_id: String,
        verification_url: String,
        user_code: String,
    },
    #[serde(rename = "amazonBedrock", rename_all = "camelCase")]
    #[ts(rename = "amazonBedrock", rename_all = "camelCase")]
    AmazonBedrock {},
}

impl fmt::Debug for AccountProfileLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ApiKey {} => "AccountProfileLogin::ApiKey",
            Self::Chatgpt { .. } => "AccountProfileLogin::Chatgpt([redacted])",
            Self::ChatgptDeviceCode { .. } => "AccountProfileLogin::ChatgptDeviceCode([redacted])",
            Self::AmazonBedrock {} => "AccountProfileLogin::AmazonBedrock",
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileLoginCancelParams {
    pub login_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileLoginCancelResponse {
    pub status: CancelLoginAccountStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileRateLimitReadParams {
    pub account_id: String,
    #[ts(optional = nullable)]
    pub limit_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileRateLimitReadResponse {
    pub account_id: String,
    pub data: Vec<RateLimitSnapshot>,
    pub observed_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountAutoSelectionReadParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountAutoSelectionReadResponse {
    pub auto_selection: AccountAutoSelection,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountAutoSelectionWriteParams {
    pub enabled: bool,
    pub policy: AccountAutoSelectionPolicy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountAutoSelectionWriteResponse {
    pub auto_selection: AccountAutoSelection,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountProfileActiveChangedNotification {
    pub account_id: String,
    pub previous_account_id: Option<String>,
    pub changed_at: i64,
    pub generation: u64,
}
