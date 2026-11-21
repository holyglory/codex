//! Account controls follow the app-server handshake, including remote servers.

use super::AccountProfilesRequest;
use super::ChatWidget;
use codex_app_server_protocol::MultiAccountCapability;

impl ChatWidget {
    pub(crate) fn set_account_profiles_capability(
        &mut self,
        capability: Option<&MultiAccountCapability>,
    ) {
        self.account_profiles_capability = capability
            .filter(|capability| capability.version == 2)
            .cloned();
        self.bottom_pane
            .set_account_command_enabled(self.account_profiles_supported());
    }

    pub(crate) fn account_profiles_capability(&self) -> Option<&MultiAccountCapability> {
        self.account_profiles_capability.as_ref()
    }

    pub(crate) fn account_profiles_supported(&self) -> bool {
        self.account_profiles_capability.is_some()
    }

    pub(super) fn account_profiles_managed_login_supported(&self) -> bool {
        self.account_profiles_capability
            .as_ref()
            .is_some_and(|capability| capability.supports_managed_login)
    }

    pub(super) fn account_profiles_auto_selection_supported(&self) -> bool {
        self.account_profiles_capability
            .as_ref()
            .is_some_and(|capability| capability.supports_auto_selection)
    }
}

pub(crate) fn account_profiles_request_supported(
    capability: Option<&MultiAccountCapability>,
    request: &AccountProfilesRequest,
) -> bool {
    let Some(capability) = capability.filter(|capability| capability.version == 2) else {
        return false;
    };
    match request {
        AccountProfilesRequest::List { .. }
        | AccountProfilesRequest::Activate { .. }
        | AccountProfilesRequest::Update(_)
        | AccountProfilesRequest::Remove { .. }
        | AccountProfilesRequest::Limits { .. } => true,
        AccountProfilesRequest::AutoRead | AccountProfilesRequest::AutoWrite { .. } => {
            capability.supports_auto_selection
        }
        AccountProfilesRequest::LoginStart { .. } | AccountProfilesRequest::LoginCancel { .. } => {
            capability.supports_managed_login
        }
    }
}
