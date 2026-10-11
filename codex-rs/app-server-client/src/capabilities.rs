use codex_app_server_protocol::LocalUsageAccountingCapability;
use codex_app_server_protocol::MultiAccountCapability;
use serde::Deserialize;

const MULTI_ACCOUNT_VERSION: u32 = 2;
const LOCAL_USAGE_ACCOUNTING_VERSION: u32 = 2;

/// Capabilities advertised by the app-server during its initialize handshake.
///
/// Each capability is retained only when the client understands its advertised
/// protocol version. Missing, malformed, and unknown-version advertisements
/// remain unavailable so callers do not optimistically send unsupported
/// requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppServerCapabilities {
    multi_account: Option<MultiAccountCapability>,
    local_usage_accounting: Option<LocalUsageAccountingCapability>,
}

impl AppServerCapabilities {
    /// Capabilities provided by the current in-process app-server contract.
    ///
    /// In-process startup completes the server-owned initialize exchange before
    /// returning its handle, but the low-level runtime does not expose that
    /// response to the facade. Keep this versioned local contract aligned with
    /// the server implementation without issuing a second initialize request.
    pub(crate) fn advertised_v1() -> Self {
        Self::from_advertised(
            Some(MultiAccountCapability {
                version: MULTI_ACCOUNT_VERSION,
                supports_managed_login: true,
                supports_auto_selection: true,
                supports_credit_usage: true,
            }),
            Some(LocalUsageAccountingCapability {
                version: LOCAL_USAGE_ACCOUNTING_VERSION,
            }),
        )
    }

    pub(crate) fn from_advertised(
        multi_account: Option<MultiAccountCapability>,
        local_usage_accounting: Option<LocalUsageAccountingCapability>,
    ) -> Self {
        Self {
            multi_account: multi_account
                .filter(|capability| capability.version == MULTI_ACCOUNT_VERSION),
            local_usage_accounting: local_usage_accounting
                .filter(|capability| capability.version == LOCAL_USAGE_ACCOUNTING_VERSION),
        }
    }

    pub(crate) fn from_initialize_result(result: &serde_json::Value) -> Self {
        let advertised = serde_json::from_value::<InitializeCapabilityFields>(result.clone())
            .unwrap_or_default();
        Self::from_advertised(advertised.multi_account, advertised.local_usage_accounting)
    }

    pub fn multi_account(&self) -> Option<&MultiAccountCapability> {
        self.multi_account.as_ref()
    }

    pub fn local_usage_accounting(&self) -> Option<&LocalUsageAccountingCapability> {
        self.local_usage_accounting.as_ref()
    }

    pub fn supports_multi_account(&self) -> bool {
        self.multi_account.is_some()
    }

    pub fn supports_local_usage_accounting(&self) -> bool {
        self.local_usage_accounting.is_some()
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeCapabilityFields {
    #[serde(default)]
    multi_account: Option<MultiAccountCapability>,
    #[serde(default)]
    local_usage_accounting: Option<LocalUsageAccountingCapability>,
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;
