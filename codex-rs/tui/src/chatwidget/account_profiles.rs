use super::*;
use crate::bottom_pane::SelectionDescriptionLayout;
use codex_app_server_protocol::AccountAutoSelection;
use codex_app_server_protocol::AccountAutoSelectionPolicy;
use codex_app_server_protocol::AccountAutoSelectionReadResponse;
use codex_app_server_protocol::AccountAutoSelectionWriteResponse;
use codex_app_server_protocol::AccountPriorityOrder;
use codex_app_server_protocol::AccountProfile;
use codex_app_server_protocol::AccountProfileActivateResponse;
use codex_app_server_protocol::AccountProfileLoginStartResponse;
use codex_app_server_protocol::AccountProfileRateLimitReadResponse;
use codex_app_server_protocol::AccountProfileRemoveResponse;
use codex_app_server_protocol::AccountProfileUpdateParams;
use codex_app_server_protocol::AccountProfileUpdateResponse;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::RateLimitSnapshot;
use std::collections::BTreeMap;

#[path = "account_profiles_format.rs"]
mod format;
use format::*;
#[path = "account_profiles_edit.rs"]
mod edit;

const ACCOUNT_VIEW_ID: &str = "account-profiles";
const MAX_INLINE_NOTICE_BYTES: usize = 36;

fn account_popup_hint_line() -> Line<'static> {
    Line::from(vec![
        key_hint::plain(KeyCode::Enter).into(),
        " confirm · ".into(),
        key_hint::plain(KeyCode::Esc).into(),
        " back".into(),
    ])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountPostListAction {
    Show,
    Reveal { account_id: String, notice: String },
    ShowWithNotice(String),
    Use(String),
    Edit(String),
    Remove(String),
    Limits(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountLoginChoice {
    Browser,
    Device,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountProfilesRequest {
    List {
        cursor: Option<String>,
        after: AccountPostListAction,
    },
    Activate {
        account_id: String,
    },
    Update(AccountProfileUpdateParams),
    Remove {
        account_id: String,
    },
    Limits {
        account_id: String,
    },
    AutoRead,
    AutoWrite {
        enabled: bool,
    },
    LoginStart {
        alias: String,
        choice: AccountLoginChoice,
    },
    LoginCancel {
        login_id: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccountProfilesListData {
    pub(crate) profiles: Vec<AccountProfile>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) limits: BTreeMap<String, Option<AccountProfileRateLimitReadResponse>>,
    pub(crate) auto_selection: Option<AccountAutoSelection>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AccountProfilesResponse {
    List(AccountProfilesListData),
    Activated(AccountProfileActivateResponse),
    Updated(AccountProfileUpdateResponse),
    Removed(AccountProfileRemoveResponse),
    Limits(AccountProfileRateLimitReadResponse),
    AutoRead(AccountAutoSelectionReadResponse),
    AutoWrite(AccountAutoSelectionWriteResponse),
    LoginStarted(AccountProfileLoginStartResponse),
    LoginCancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccountProfilesError {
    AliasInvalid,
    AliasInUse,
    NoteInvalid,
    PriorityInvalid,
    Unavailable,
}

impl ChatWidget {
    pub(crate) fn set_account_profiles_supported(&mut self, supported: bool) {
        self.account_profiles_supported = supported;
        self.bottom_pane.set_account_command_enabled(supported);
    }

    pub(crate) fn account_profiles_supported(&self) -> bool {
        self.account_profiles_supported
    }

    pub(crate) fn refresh_account_profiles_after_login(&mut self, success: bool) {
        self.refresh_account_profiles_if_idle(Some(if success {
            "Account sign-in completed.".to_string()
        } else {
            "Account sign-in failed.".to_string()
        }));
    }

    pub(crate) fn refresh_account_profiles_after_active_change(&mut self) {
        self.refresh_account_profiles_if_idle(/*notice*/ None);
    }

    fn refresh_account_profiles_if_idle(&mut self, notice: Option<String>) {
        if self.account_profiles_supported
            && self.pending_account_profiles_request.is_none()
            && self
                .bottom_pane
                .selected_index_for_active_view(ACCOUNT_VIEW_ID)
                .is_some()
        {
            self.dismiss_account_views();
            self.open_account_profiles_request(AccountProfilesRequest::List {
                cursor: None,
                after: notice.map_or(
                    AccountPostListAction::Show,
                    AccountPostListAction::ShowWithNotice,
                ),
            });
        }
    }

    pub(super) fn open_account_command(&mut self, args: &str) {
        if !self.account_profiles_supported {
            self.add_error_message("Account profiles are unavailable on this server.".to_string());
            return;
        }
        let tokens = args.split_whitespace().collect::<Vec<_>>();
        let request = match tokens.as_slice() {
            [] | ["list"] => AccountProfilesRequest::List {
                cursor: None,
                after: AccountPostListAction::Show,
            },
            ["use", reference] => AccountProfilesRequest::List {
                cursor: None,
                after: AccountPostListAction::Use((*reference).to_string()),
            },
            ["add", alias] => {
                self.show_account_login_choice((*alias).to_string());
                return;
            }
            ["edit", reference] => AccountProfilesRequest::List {
                cursor: None,
                after: AccountPostListAction::Edit((*reference).to_string()),
            },
            ["remove", reference] => AccountProfilesRequest::List {
                cursor: None,
                after: AccountPostListAction::Remove((*reference).to_string()),
            },
            ["limits"] => AccountProfilesRequest::List {
                cursor: None,
                after: AccountPostListAction::Limits(None),
            },
            ["limits", reference] => AccountProfilesRequest::List {
                cursor: None,
                after: AccountPostListAction::Limits(Some((*reference).to_string())),
            },
            ["auto"] => AccountProfilesRequest::AutoRead,
            _ => {
                self.add_error_message(
                    "Usage: /account [list|use <ref>|add <alias>|edit <ref>|remove <ref>|limits [ref]|auto]"
                        .to_string(),
                );
                return;
            }
        };
        self.open_account_profiles_request(request);
    }

    pub(crate) fn open_account_profiles_request(&mut self, request: AccountProfilesRequest) {
        let request_id = self.next_account_profiles_request_id;
        self.next_account_profiles_request_id =
            self.next_account_profiles_request_id.wrapping_add(1);
        self.pending_account_profiles_request = Some((request_id, request.clone()));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some(request.title().to_string()),
            items: vec![SelectionItem {
                name: "Loading account profiles...".to_string(),
                is_disabled: true,
                ..Default::default()
            }],
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.app_event_tx.send(AppEvent::RefreshAccountProfiles {
            request_id,
            request,
        });
        self.request_redraw();
    }

    pub(crate) fn finish_account_profiles_request(
        &mut self,
        request_id: u64,
        request: AccountProfilesRequest,
        result: Result<AccountProfilesResponse, AccountProfilesError>,
    ) {
        if self
            .pending_account_profiles_request
            .as_ref()
            .is_none_or(|pending| pending.0 != request_id || pending.1 != request)
        {
            return;
        }
        self.pending_account_profiles_request = None;
        self.bottom_pane.dismiss_view_by_id(ACCOUNT_VIEW_ID);
        match result {
            Ok(AccountProfilesResponse::List(data)) => {
                self.account_auto_selection_enabled = data
                    .auto_selection
                    .as_ref()
                    .map(|selection| selection.enabled);
                self.account_profiles = data.profiles.clone();
                self.finish_account_list(request, data);
            }
            Ok(AccountProfilesResponse::Activated(response)) => {
                let profile = response.profile;
                let notice = if profile.auth_mode == AuthMode::Chatgpt
                    || self.account_auto_selection_enabled == Some(false)
                {
                    format!(
                        "{} will be used for the next turn; the running turn keeps its current account.",
                        profile.alias
                    )
                } else if self.account_auto_selection_enabled == Some(true) {
                    format!(
                        "{} is now the default account, but automatic selection uses only locally managed ChatGPT OAuth profiles. Turn automatic selection off or start Codex with --account {} to use this profile.",
                        profile.alias, profile.alias
                    )
                } else {
                    format!(
                        "{} is now the default account, but automatic selection status is unavailable. Turn automatic selection off or start Codex with --account {} to ensure this profile is used.",
                        profile.alias, profile.alias
                    )
                };
                self.dismiss_account_views();
                self.open_account_profiles_request(AccountProfilesRequest::List {
                    cursor: None,
                    after: AccountPostListAction::Reveal {
                        account_id: profile.id,
                        notice,
                    },
                });
            }
            Ok(AccountProfilesResponse::Updated(response)) => {
                self.dismiss_account_views();
                self.open_account_profiles_request(AccountProfilesRequest::List {
                    cursor: None,
                    after: AccountPostListAction::Reveal {
                        account_id: response.profile.id,
                        notice: format!("{} was updated.", response.profile.alias),
                    },
                });
            }
            Ok(AccountProfilesResponse::Removed(_)) => {
                self.dismiss_account_views();
                self.open_account_profiles_request(AccountProfilesRequest::List {
                    cursor: None,
                    after: AccountPostListAction::ShowWithNotice("Account removed.".to_string()),
                });
            }
            Ok(AccountProfilesResponse::Limits(response)) => {
                self.show_account_limits(&response.data, AccountPostListAction::Show);
            }
            Ok(AccountProfilesResponse::AutoRead(response)) => {
                self.account_auto_selection_enabled = Some(response.auto_selection.enabled);
                self.show_account_auto(response.auto_selection);
            }
            Ok(AccountProfilesResponse::AutoWrite(response)) => {
                self.account_auto_selection_enabled = Some(response.auto_selection.enabled);
                let policy = auto_policy_name(response.auto_selection.policy);
                let priority_order =
                    priority_order_description(response.auto_selection.priority_order);
                self.dismiss_account_views();
                self.open_account_profiles_request(AccountProfilesRequest::List {
                    cursor: None,
                    after: AccountPostListAction::ShowWithNotice(format!(
                        "Automatic selection is {} (policy: {policy}; {priority_order}).",
                        if response.auto_selection.enabled {
                            "on"
                        } else {
                            "off"
                        }
                    )),
                });
            }
            Ok(AccountProfilesResponse::LoginStarted(response)) => {
                self.dismiss_account_views();
                self.show_account_login_started(response);
            }
            Ok(AccountProfilesResponse::LoginCancelled) => {
                self.dismiss_account_views();
                self.open_account_profiles_request(AccountProfilesRequest::List {
                    cursor: None,
                    after: AccountPostListAction::Show,
                });
            }
            Err(error) => self.show_account_error(request, error),
        }
    }

    fn dismiss_account_views(&mut self) {
        while self.bottom_pane.dismiss_view_by_id(ACCOUNT_VIEW_ID) {}
    }

    fn finish_account_list(
        &mut self,
        request: AccountProfilesRequest,
        data: AccountProfilesListData,
    ) {
        let after = match request {
            AccountProfilesRequest::List { after, .. } => after,
            _ => AccountPostListAction::Show,
        };
        match after {
            AccountPostListAction::Show => self.show_account_list(data),
            AccountPostListAction::Reveal { account_id, notice } => {
                self.show_account_list_with_context(data, Some(&account_id), Some(notice));
            }
            AccountPostListAction::ShowWithNotice(notice) => {
                self.show_account_list_with_context(
                    data,
                    /*reveal_account_id*/ None,
                    Some(notice),
                );
            }
            AccountPostListAction::Use(reference) => {
                if let Some(profile) = resolve_profile(&data.profiles, &reference) {
                    self.open_account_profiles_request(AccountProfilesRequest::Activate {
                        account_id: profile.id.clone(),
                    });
                } else {
                    self.show_account_message(
                        "Account not found",
                        "No matching profile.".to_string(),
                    );
                }
            }
            AccountPostListAction::Edit(reference) => {
                if let Some(profile) = resolve_profile(&data.profiles, &reference).cloned() {
                    self.show_account_list_with_context(
                        data,
                        Some(&profile.id),
                        /*notice*/ None,
                    );
                    self.show_account_actions(profile);
                } else {
                    self.show_account_message(
                        "Account not found",
                        "No matching profile.".to_string(),
                    );
                }
            }
            AccountPostListAction::Remove(reference) => {
                if let Some(profile) = resolve_profile(&data.profiles, &reference).cloned() {
                    self.show_account_list_with_context(
                        data,
                        Some(&profile.id),
                        /*notice*/ None,
                    );
                    self.show_account_remove_confirmation(profile);
                } else {
                    self.show_account_message(
                        "Account not found",
                        "No matching profile.".to_string(),
                    );
                }
            }
            AccountPostListAction::Limits(reference) => match reference {
                Some(reference) => {
                    if let Some(profile) = resolve_profile(&data.profiles, &reference) {
                        self.open_account_profiles_request(AccountProfilesRequest::Limits {
                            account_id: profile.id.clone(),
                        });
                    } else {
                        self.show_account_message(
                            "Account not found",
                            "No matching profile.".to_string(),
                        );
                    }
                }
                None => {
                    let selector_data = data.clone();
                    self.show_account_list(data);
                    self.show_account_limit_selector(selector_data);
                }
            },
        }
    }

    fn show_account_list(&mut self, data: AccountProfilesListData) {
        self.show_account_list_with_context(
            data, /*reveal_account_id*/ None, /*notice*/ None,
        );
    }

    fn show_account_list_with_context(
        &mut self,
        data: AccountProfilesListData,
        reveal_account_id: Option<&str>,
        notice: Option<String>,
    ) {
        let initial_selected_idx = reveal_account_id.and_then(|account_id| {
            data.profiles
                .iter()
                .position(|profile| profile.id == account_id)
        });
        let mut items = data
            .profiles
            .iter()
            .map(|profile| {
                account_item(
                    profile,
                    data.limits.get(&profile.id).and_then(Option::as_ref),
                )
            })
            .collect::<Vec<_>>();
        items.push(SelectionItem {
            name: "Add account".to_string(),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenAccountAliasPrompt {
                    value: None,
                    error: None,
                })
            })],
            dismiss_on_select: false,
            ..Default::default()
        });
        let auto_selection = data.auto_selection;
        items.push(SelectionItem {
            name: "Automatic selection".to_string(),
            description: Some(auto_selection.as_ref().map_or_else(
                || "Status unavailable".to_string(),
                |auto_selection| {
                    format!(
                        "{} · policy: {} · {}",
                        if auto_selection.enabled { "on" } else { "off" },
                        auto_policy_name(auto_selection.policy),
                        priority_order_description(auto_selection.priority_order),
                    )
                },
            )),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenAccountProfiles {
                    request: AccountProfilesRequest::AutoRead,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        if let Some(cursor) = data.next_cursor {
            items.push(SelectionItem {
                name: "Next page".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: AccountProfilesRequest::List {
                            cursor: Some(cursor.clone()),
                            after: AccountPostListAction::Show,
                        },
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        let (subtitle, footer_note) = match notice {
            Some(notice) if notice.len() > MAX_INLINE_NOTICE_BYTES => {
                (None, Some(Line::from(notice.dim())))
            }
            Some(notice) => (Some(notice), None),
            None => (
                data.profiles
                    .is_empty()
                    .then(|| "No account profiles configured.".to_string()),
                None,
            ),
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some("Accounts".to_string()),
            subtitle,
            footer_note,
            items,
            is_searchable: true,
            search_placeholder: Some("Search accounts".to_string()),
            initial_selected_idx,
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_account_actions(&mut self, profile: AccountProfile) {
        let use_profile = profile.clone();
        let alias_profile = profile.clone();
        let note_profile = profile.clone();
        let clear_note_profile = profile.clone();
        let priority_profile = profile.clone();
        let toggle_profile = profile.clone();
        let limits_profile = profile.clone();
        let remove_profile = profile.clone();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some(profile.alias.clone()),
            items: vec![
                SelectionItem {
                    name: "Account status".to_string(),
                    description: Some(profile_description(&profile, /*limits*/ None)),
                    is_disabled: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Use for next turn".to_string(),
                    is_disabled: profile.is_active || !profile.enabled || !profile.authenticated,
                    disabled_reason: (!profile.authenticated)
                        .then(|| "Sign in before selecting this account.".to_string()),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Activate {
                                account_id: use_profile.id.clone(),
                            },
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Edit alias".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountAliasEditor {
                            profile: alias_profile.clone(),
                            value: None,
                            error: None,
                        })
                    })],
                    dismiss_on_select: false,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Edit priority".to_string(),
                    description: Some(
                        "Higher numbers drain first; smaller numbers drain last.".to_string(),
                    ),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountPriorityEditor {
                            profile: priority_profile.clone(),
                            value: None,
                            error: None,
                        })
                    })],
                    dismiss_on_select: false,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Edit note".to_string(),
                    description: Some(
                        profile
                            .note
                            .clone()
                            .unwrap_or_else(|| "No note".to_string()),
                    ),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountNoteEditor {
                            profile: note_profile.clone(),
                            value: None,
                            error: None,
                        })
                    })],
                    dismiss_on_select: false,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Clear note".to_string(),
                    is_disabled: profile.note.is_none(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Update(AccountProfileUpdateParams {
                                account_id: clear_note_profile.id.clone(),
                                alias: None,
                                enabled: None,
                                priority: None,
                                note: None,
                                clear_note: true,
                            }),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: if profile.enabled { "Disable" } else { "Enable" }.to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Update(AccountProfileUpdateParams {
                                account_id: toggle_profile.id.clone(),
                                alias: None,
                                enabled: Some(!toggle_profile.enabled),
                                priority: None,
                                note: None,
                                clear_note: false,
                            }),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "View limits".to_string(),
                    is_disabled: !profile.authenticated,
                    disabled_reason: (!profile.authenticated)
                        .then(|| "Sign in to read limits.".to_string()),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Limits {
                                account_id: limits_profile.id.clone(),
                            },
                        })
                    })],
                    dismiss_on_select: false,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Remove account".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountRemoveConfirmation {
                            profile: remove_profile.clone(),
                        })
                    })],
                    dismiss_on_select: false,
                    ..Default::default()
                },
            ],
            footer_hint: Some(account_popup_hint_line()),
            description_layout: SelectionDescriptionLayout::StackBelowWhenNarrow {
                min_description_width: 28,
            },
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_account_remove_confirmation(&mut self, profile: AccountProfile) {
        let remove = profile.clone();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some(format!("Remove {}?", profile.alias)),
            subtitle: Some("This removes the profile and its stored credentials.".to_string()),
            initial_selected_idx: Some(1),
            items: vec![
                SelectionItem {
                    name: format!("Remove {}", profile.alias),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Remove {
                                account_id: remove.id.clone(),
                            },
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_account_login_choice(&mut self, alias: String) {
        let browser_alias = alias.clone();
        let device_alias = alias.clone();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some(format!("Add {alias}")),
            initial_selected_idx: Some(0),
            items: vec![
                SelectionItem {
                    name: "Continue in browser".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::LoginStart {
                                alias: browser_alias.clone(),
                                choice: AccountLoginChoice::Browser,
                            },
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Use device code".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::LoginStart {
                                alias: device_alias.clone(),
                                choice: AccountLoginChoice::Device,
                            },
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    actions: vec![Box::new(|tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::List {
                                cursor: None,
                                after: AccountPostListAction::Show,
                            },
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    fn show_account_login_started(&mut self, response: AccountProfileLoginStartResponse) {
        use codex_app_server_protocol::AccountProfileLogin;
        match response.login {
            AccountProfileLogin::Chatgpt { login_id, auth_url } => {
                self.app_event_tx
                    .send(AppEvent::OpenUrlInBrowser { url: auth_url });
                self.show_login_waiting(login_id, "Complete sign-in in your browser.".to_string());
            }
            AccountProfileLogin::ChatgptDeviceCode {
                login_id,
                verification_url,
                user_code,
            } => {
                self.show_login_waiting(
                    login_id,
                    format!("Open {verification_url} and enter code {user_code}."),
                );
            }
            AccountProfileLogin::ApiKey {} | AccountProfileLogin::AmazonBedrock {} => {
                self.open_account_profiles_request(AccountProfilesRequest::List {
                    cursor: None,
                    after: AccountPostListAction::ShowWithNotice(
                        "Account profile added.".to_string(),
                    ),
                });
            }
        }
    }

    fn show_login_waiting(&mut self, login_id: String, message: String) {
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some("Account sign-in".to_string()),
            subtitle: Some(message),
            items: vec![SelectionItem {
                name: "Cancel sign-in".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: AccountProfilesRequest::LoginCancel {
                            login_id: login_id.clone(),
                        },
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            }],
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    fn show_account_limit_selector(&mut self, data: AccountProfilesListData) {
        let mut items = data
            .profiles
            .into_iter()
            .map(|profile| SelectionItem {
                name: profile.alias,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: AccountProfilesRequest::Limits {
                            account_id: profile.id.clone(),
                        },
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            items.push(disabled_item("No account profiles configured.".to_string()));
        }
        items.push(SelectionItem {
            name: "Back".to_string(),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenAccountProfiles {
                    request: AccountProfilesRequest::List {
                        cursor: None,
                        after: AccountPostListAction::Show,
                    },
                })
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some("Account limits".to_string()),
            items,
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    fn show_account_limits(&mut self, limits: &[RateLimitSnapshot], _back: AccountPostListAction) {
        let mut items = vec![disabled_item("Limits: service reported".to_string())];
        if limits.is_empty() {
            items.push(disabled_item("Limit status: unknown".to_string()));
        } else {
            for (index, snapshot) in limits.iter().enumerate() {
                let name = snapshot
                    .limit_name
                    .clone()
                    .unwrap_or_else(|| format!("Limit {}", index + 1));
                items.push(disabled_item(format!(
                    "{name}: {}",
                    rate_limit_summary(snapshot)
                )));
            }
        }
        items.push(SelectionItem {
            name: "Back".to_string(),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenAccountProfiles {
                    request: AccountProfilesRequest::List {
                        cursor: None,
                        after: AccountPostListAction::Show,
                    },
                })
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some("Account limits".to_string()),
            items,
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    fn show_account_auto(&mut self, auto_selection: AccountAutoSelection) {
        let enabled = auto_selection.enabled;
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some("Automatic account selection".to_string()),
            initial_selected_idx: Some(1),
            items: vec![
                disabled_item(format!("Status: {}", if enabled { "on" } else { "off" })),
                SelectionItem {
                    name: if enabled { "Turn off" } else { "Turn on" }.to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::AutoWrite { enabled: !enabled },
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                disabled_item(format!(
                    "Policy: {}",
                    auto_policy_name(auto_selection.policy)
                )),
                disabled_item(format!(
                    "Priority order: {}",
                    priority_order_description(auto_selection.priority_order)
                )),
                disabled_item("Eligible: Managed ChatGPT OAuth only".to_string()),
                disabled_item("Stale/unknown limits".to_string()),
                disabled_item("Never auto-selected".to_string()),
            ],
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    fn show_account_message(&mut self, title: &str, message: String) {
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some(title.to_string()),
            subtitle: Some(message),
            items: vec![SelectionItem {
                name: "Back to accounts".to_string(),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: AccountProfilesRequest::List {
                            cursor: None,
                            after: AccountPostListAction::Show,
                        },
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            }],
            footer_hint: Some(account_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }
}

fn auto_policy_name(policy: AccountAutoSelectionPolicy) -> &'static str {
    match policy {
        AccountAutoSelectionPolicy::Priority => "Priority",
    }
}

fn priority_order_description(order: AccountPriorityOrder) -> &'static str {
    match order {
        AccountPriorityOrder::HigherFirst => {
            "higher numbers drain first; smaller numbers drain last"
        }
    }
}
