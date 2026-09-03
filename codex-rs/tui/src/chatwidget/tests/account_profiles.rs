use super::*;
use crate::bottom_pane::slash_commands::BuiltinCommandFlags;
use crate::bottom_pane::slash_commands::builtins_for_input;
use codex_app_server_protocol::AccountAutoSelection;
use codex_app_server_protocol::AccountAutoSelectionPolicy;
use codex_app_server_protocol::AccountAutoSelectionReadResponse;
use codex_app_server_protocol::AccountPriorityOrder;
use codex_app_server_protocol::AccountProfile;
use codex_app_server_protocol::AccountProfileActivateResponse;
use codex_app_server_protocol::AccountProfileListResponse;
use codex_app_server_protocol::AccountProfileRateLimitReadResponse;
use codex_app_server_protocol::AccountProfileUpdateParams;
use codex_app_server_protocol::AccountProfileUpdateResponse;
use codex_app_server_protocol::AuthMode;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn profile(alias: &str, active: bool) -> AccountProfile {
    AccountProfile {
        id: format!("private-id-{alias}"),
        alias: alias.to_string(),
        auth_mode: AuthMode::Chatgpt,
        email: Some(format!("{alias}@example.com")),
        plan_type: Some(PlanType::Plus),
        enabled: true,
        authenticated: true,
        priority: 10,
        created_at: 1,
        last_used_at: Some(2),
        note: None,
        is_default: active,
        is_active: active,
    }
}

fn list_data(profiles: Vec<AccountProfile>) -> AccountProfilesListData {
    let limits = profiles
        .iter()
        .map(|profile| {
            (
                profile.id.clone(),
                Some(AccountProfileRateLimitReadResponse {
                    account_id: profile.id.clone(),
                    data: Vec::new(),
                    observed_at: None,
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    AccountProfilesListData {
        profiles,
        next_cursor: None,
        limits,
        auto_selection: Some(AccountAutoSelection {
            enabled: false,
            policy: AccountAutoSelectionPolicy::Priority,
            priority_order: AccountPriorityOrder::HigherFirst,
        }),
    }
}

fn normalized_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn account_selector_populated_wide_and_narrow_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    chat.open_account_command("");
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("expected account list request");
    };
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(list_data({
            let mut logged_out = profile("logged-out", /*active*/ false);
            logged_out.authenticated = false;
            vec![
                profile("primary", /*active*/ true),
                profile("backup", /*active*/ false),
                logged_out,
            ]
        }))),
    );

    let wide_list = render_bottom_popup(&chat, /*width*/ 96);
    let narrow_list = render_bottom_popup(&chat, /*width*/ 40);
    assert!(!wide_list.contains("private-id"));
    assert!(!narrow_list.contains("private-id"));
    assert!(wide_list.contains("off · policy: Priority"));
    let normalized_wide_list = normalized_text(&wide_list);
    assert!(
        normalized_wide_list.contains("higher numbers drain first; smaller numbers drain last")
    );
    assert!(normalized_wide_list.contains("logged out"));
    let mut noted = profile("noted", /*active*/ false);
    noted.authenticated = false;
    noted.note = Some(
        "Prefer this profile for long-running review and documentation tasks when limits permit."
            .to_string(),
    );
    chat.show_account_actions(noted);
    let wide = [wide_list, render_bottom_popup(&chat, /*width*/ 96)].join("\n---\n");
    let narrow = [narrow_list, render_bottom_popup(&chat, /*width*/ 40)].join("\n---\n");
    assert_chatwidget_snapshot!("account_selector_populated_wide", wide);
    assert_chatwidget_snapshot!("account_selector_populated_narrow", narrow);
}

#[tokio::test]
async fn account_selector_empty_error_and_long_alias_states_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    let request = AccountProfilesRequest::List {
        cursor: None,
        after: AccountPostListAction::Show,
    };
    chat.open_account_profiles_request(request.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected account request");
    };
    let mut empty_data = list_data(Vec::new());
    empty_data.auto_selection = None;
    let loading = render_bottom_popup(&chat, /*width*/ 72);
    chat.finish_account_profiles_request(
        request_id,
        request.clone(),
        Ok(AccountProfilesResponse::List(empty_data)),
    );
    let empty = render_bottom_popup(&chat, /*width*/ 72);
    assert!(empty.contains("Status unavailable"));

    chat.open_account_profiles_request(request.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected account request");
    };
    chat.finish_account_profiles_request(
        request_id,
        request.clone(),
        Err(AccountProfilesError::Unavailable),
    );
    let error = render_bottom_popup(&chat, /*width*/ 72);
    assert!(!error.contains("private backend error"));

    chat.open_account_profiles_request(request.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected account request");
    };
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(list_data(vec![profile(
            "a-very-long-account-alias-that-needs-wrapping",
            /*active*/ true,
        )]))),
    );
    let long = render_bottom_popup(&chat, /*width*/ 52);
    assert!(!long.contains("private-id"));
    assert_chatwidget_snapshot!(
        "account_selector_loading_empty_error_long",
        [loading, empty, error, long].join("\n---\n")
    );
}

#[tokio::test]
async fn activation_remove_auto_and_validation_actions_are_functional() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    let candidate = profile("candidate", /*active*/ false);
    chat.show_account_actions(candidate.clone());
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::OpenAccountProfiles {
            request: AccountProfilesRequest::Activate { account_id }
        }) if account_id == candidate.id
    );

    chat.show_account_remove_confirmation(candidate);
    let rendered = render_bottom_popup(&chat, /*width*/ 72);
    assert!(rendered.contains("Remove candidate?"));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(rx.try_recv().is_err(), "cancel must not remove the account");

    chat.open_account_profiles_request(AccountProfilesRequest::AutoRead);
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("expected auto-selection request");
    };
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::AutoRead(
            AccountAutoSelectionReadResponse {
                auto_selection: AccountAutoSelection {
                    enabled: false,
                    policy: AccountAutoSelectionPolicy::Priority,
                    priority_order: AccountPriorityOrder::HigherFirst,
                },
            },
        )),
    );
    let auto = render_bottom_popup(&chat, /*width*/ 72);
    assert_chatwidget_snapshot!("account_auto_policy_wide", auto);
    assert_chatwidget_snapshot!(
        "account_auto_policy_narrow",
        render_bottom_popup(&chat, /*width*/ 40)
    );
    let normalized_auto = normalized_text(&auto);
    assert!(normalized_auto.contains("Priority"));
    assert!(normalized_auto.contains("higher numbers drain first"));
    assert!(normalized_auto.contains("smaller numbers drain last"));
    assert!(normalized_auto.contains("Managed ChatGPT OAuth only"));
    assert!(normalized_auto.contains("Stale/unknown limits"));
    assert!(normalized_auto.contains("Never auto-selected"));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::OpenAccountProfiles {
            request: AccountProfilesRequest::AutoWrite { enabled: true }
        })
    );
}

#[tokio::test]
async fn note_edit_clear_and_cancel_restore_account_context() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut candidate = profile("notes", /*active*/ false);
    candidate.note = Some("existing note".to_string());

    chat.show_account_actions(candidate.clone());
    chat.show_account_note_editor(
        candidate.clone(),
        Some("updated note".to_string()),
        /*error*/ None,
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::OpenAccountProfiles {
            request: AccountProfilesRequest::Update(params)
        }) if params.account_id == candidate.id
            && params.note.as_deref() == Some("updated note")
            && !params.clear_note
    );

    chat.show_account_actions(candidate.clone());
    for _ in 0..4 {
        chat.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::OpenAccountProfiles {
            request: AccountProfilesRequest::Update(params)
        }) if params.account_id == candidate.id && params.note.is_none() && params.clear_note
    );

    chat.show_account_actions(candidate.clone());
    chat.show_account_note_editor(candidate.clone(), Some(String::new()), /*error*/ None);
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::OpenAccountProfiles {
            request: AccountProfilesRequest::Update(params)
        }) if params.account_id == candidate.id && params.note.is_none() && params.clear_note
    );

    chat.show_account_actions(candidate.clone());
    chat.show_account_note_editor(candidate, /*value*/ None, /*error*/ None);
    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let restored = render_bottom_popup(&chat, /*width*/ 64);
    assert!(restored.contains("notes"));
    assert!(restored.contains("Edit note"));
    assert!(rx.try_recv().is_err(), "cancel must not update the account");
}

#[tokio::test]
async fn mutation_validation_returns_to_the_correct_editor_with_input() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let candidate = profile("correctable", /*active*/ false);
    chat.account_profiles = vec![candidate.clone()];

    let alias_request = AccountProfilesRequest::Update(AccountProfileUpdateParams {
        account_id: candidate.id.clone(),
        alias: Some("INVALID-alias".to_string()),
        enabled: None,
        priority: None,
        note: None,
        clear_note: false,
    });
    chat.open_account_profiles_request(alias_request.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected alias update request");
    };
    chat.finish_account_profiles_request(
        request_id,
        alias_request,
        Err(AccountProfilesError::AliasInvalid),
    );
    let alias = render_bottom_popup(&chat, /*width*/ 72);
    assert!(alias.contains("INVALID-alias"));
    assert!(alias.contains("Lowercase letters"));

    let note_request = AccountProfilesRequest::Update(AccountProfileUpdateParams {
        account_id: candidate.id.clone(),
        alias: None,
        enabled: None,
        priority: None,
        note: Some("correctable note".to_string()),
        clear_note: false,
    });
    chat.open_account_profiles_request(note_request.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected note update request");
    };
    chat.finish_account_profiles_request(
        request_id,
        note_request,
        Err(AccountProfilesError::NoteInvalid),
    );
    let note = render_bottom_popup(&chat, /*width*/ 52);
    assert!(note.contains("correctable note"));
    assert!(note.contains("1024 bytes"));

    let priority_request = AccountProfilesRequest::Update(AccountProfileUpdateParams {
        account_id: candidate.id,
        alias: None,
        enabled: None,
        priority: Some(42),
        note: None,
        clear_note: false,
    });
    chat.open_account_profiles_request(priority_request.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected priority update request");
    };
    chat.finish_account_profiles_request(
        request_id,
        priority_request,
        Err(AccountProfilesError::PriorityInvalid),
    );
    let priority = render_bottom_popup(&chat, /*width*/ 40);
    assert_chatwidget_snapshot!(
        "account_editor_validation_wide_narrow",
        [alias, note, priority.clone()].join("\n---\n")
    );
    let normalized_priority = normalized_text(&priority);
    assert!(normalized_priority.contains("42"));
    assert!(normalized_priority.contains("Higher first; smaller last"));
    assert!(normalized_priority.contains("u32 max"));
}

#[test]
fn account_command_is_hidden_without_capability() {
    let flags = BuiltinCommandFlags::default();
    assert!(
        !builtins_for_input(flags)
            .into_iter()
            .any(|(_, command)| command == SlashCommand::Account)
    );
    let flags = BuiltinCommandFlags {
        account_command_enabled: true,
        ..BuiltinCommandFlags::default()
    };
    assert!(
        builtins_for_input(flags)
            .into_iter()
            .any(|(_, command)| command == SlashCommand::Account)
    );
}

#[test]
fn account_list_response_shape_remains_content_free_in_ui_fixture() {
    let response = AccountProfileListResponse {
        data: vec![profile("safe-alias", /*active*/ true)],
        next_cursor: None,
    };
    assert_eq!(response.data[0].alias, "safe-alias");
}

#[tokio::test]
async fn successful_mutations_refresh_and_reveal_the_changed_profile() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    let mut changed = profile("changed", /*active*/ false);
    let activate = AccountProfilesRequest::Activate {
        account_id: changed.id.clone(),
    };
    chat.open_account_profiles_request(activate.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected activation request");
    };
    changed.is_active = true;
    chat.finish_account_profiles_request(
        request_id,
        activate,
        Ok(AccountProfilesResponse::Activated(
            AccountProfileActivateResponse {
                profile: changed.clone(),
            },
        )),
    );
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("successful activation must refresh the collection");
    };
    assert_matches!(
        &request,
        AccountProfilesRequest::List {
            after: AccountPostListAction::Reveal { account_id, .. },
            ..
        } if account_id == &changed.id
    );
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(list_data(vec![
            changed.clone(),
        ]))),
    );
    let rendered = render_bottom_popup(&chat, /*width*/ 72);
    assert!(rendered.contains("will be used for the next turn"));
    assert!(rendered.contains("changed"));

    let mut edited = changed;
    edited.alias = "edited".to_string();
    let update =
        AccountProfilesRequest::Update(codex_app_server_protocol::AccountProfileUpdateParams {
            account_id: edited.id.clone(),
            alias: Some(edited.alias.clone()),
            enabled: None,
            priority: None,
            note: None,
            clear_note: false,
        });
    chat.open_account_profiles_request(update.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected update request");
    };
    chat.finish_account_profiles_request(
        request_id,
        update,
        Ok(AccountProfilesResponse::Updated(
            AccountProfileUpdateResponse {
                profile: edited.clone(),
            },
        )),
    );
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("successful update must refresh the collection");
    };
    assert_matches!(
        &request,
        AccountProfilesRequest::List {
            after: AccountPostListAction::Reveal { .. },
            ..
        }
    );
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(list_data(vec![edited]))),
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        !chat.bottom_pane.has_active_view(),
        "closing the refreshed collection must not reveal a stale account view"
    );
}

#[tokio::test]
async fn non_chatgpt_activation_with_auto_selection_has_honest_notice_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    let mut candidate = profile("metered", /*active*/ false);
    candidate.auth_mode = AuthMode::PersonalAccessToken;

    let list = AccountProfilesRequest::List {
        cursor: None,
        after: AccountPostListAction::Show,
    };
    chat.open_account_profiles_request(list.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected account list request");
    };
    let mut initial_data = list_data(vec![candidate.clone()]);
    initial_data
        .auto_selection
        .as_mut()
        .expect("automatic-selection state")
        .enabled = true;
    chat.finish_account_profiles_request(
        request_id,
        list,
        Ok(AccountProfilesResponse::List(initial_data)),
    );

    let activate = AccountProfilesRequest::Activate {
        account_id: candidate.id.clone(),
    };
    chat.open_account_profiles_request(activate.clone());
    let Ok(AppEvent::RefreshAccountProfiles { request_id, .. }) = rx.try_recv() else {
        panic!("expected activation request");
    };
    candidate.is_default = true;
    candidate.is_active = true;
    chat.finish_account_profiles_request(
        request_id,
        activate,
        Ok(AccountProfilesResponse::Activated(
            AccountProfileActivateResponse {
                profile: candidate.clone(),
            },
        )),
    );
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("successful activation must refresh the collection");
    };
    assert_matches!(
        &request,
        AccountProfilesRequest::List {
            after: AccountPostListAction::Reveal { notice, .. },
            ..
        } if !notice.contains("will be used for the next turn")
            && notice.contains("automatic selection uses only locally managed ChatGPT OAuth profiles")
            && notice.contains("--account metered")
    );
    let mut refreshed_data = list_data(vec![candidate]);
    refreshed_data
        .auto_selection
        .as_mut()
        .expect("automatic-selection state")
        .enabled = true;
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(refreshed_data)),
    );

    assert_chatwidget_snapshot!(
        "account_non_chatgpt_activation_notice_wide",
        render_bottom_popup(&chat, /*width*/ 96)
    );
    assert_chatwidget_snapshot!(
        "account_non_chatgpt_activation_notice_narrow",
        render_bottom_popup(&chat, /*width*/ 40)
    );
}

#[tokio::test]
async fn active_change_notification_does_not_supersede_in_flight_activation() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    let mut candidate = profile("candidate", /*active*/ false);
    let activation = AccountProfilesRequest::Activate {
        account_id: candidate.id.clone(),
    };
    chat.open_account_profiles_request(activation);
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("expected activation request");
    };

    chat.refresh_account_profiles_after_active_change();
    assert!(
        rx.try_recv().is_err(),
        "active-change refresh must wait for the in-flight mutation"
    );

    candidate.is_default = true;
    candidate.is_active = true;
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::Activated(
            AccountProfileActivateResponse {
                profile: candidate.clone(),
            },
        )),
    );
    let Ok(AppEvent::RefreshAccountProfiles { request, .. }) = rx.try_recv() else {
        panic!("activation response must retain ownership of the follow-up refresh");
    };
    assert_matches!(
        request,
        AccountProfilesRequest::List {
            after: AccountPostListAction::Reveal { account_id, notice },
            ..
        } if account_id == candidate.id && notice.contains("next turn")
    );
}

#[tokio::test]
async fn failed_login_notification_refreshes_with_an_honest_notice() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_account_profiles_supported(/*supported*/ true);
    chat.open_account_command("");
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("expected account list request");
    };
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(list_data(Vec::new()))),
    );

    chat.refresh_account_profiles_after_login(/*success*/ false);
    let Ok(AppEvent::RefreshAccountProfiles {
        request_id,
        request,
    }) = rx.try_recv()
    else {
        panic!("failed login should refresh the open account view");
    };
    assert_matches!(
        &request,
        AccountProfilesRequest::List {
            after: AccountPostListAction::ShowWithNotice(notice),
            ..
        } if notice == "Account sign-in failed."
    );
    chat.finish_account_profiles_request(
        request_id,
        request,
        Ok(AccountProfilesResponse::List(list_data(Vec::new()))),
    );
    let rendered = render_bottom_popup(&chat, /*width*/ 72);
    assert_chatwidget_snapshot!("account_login_failure", rendered.clone());
    assert!(rendered.contains("Account sign-in failed."));
    assert!(!rendered.contains("Account sign-in completed."));
}
