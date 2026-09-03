use super::*;
use crate::bottom_pane::custom_prompt_view::CustomPromptView;

impl ChatWidget {
    pub(crate) fn show_account_alias_prompt(
        &mut self,
        value: Option<String>,
        error: Option<String>,
    ) {
        let tx = self.app_event_tx.clone();
        self.bottom_pane.show_text_prompt(CustomPromptView::new(
            "Add account".to_string(),
            "Account alias".to_string(),
            value.unwrap_or_default(),
            error.or_else(|| {
                Some("Enter an alias, then choose browser or device login.".to_string())
            }),
            Box::new(move |alias| {
                if valid_account_alias(&alias) {
                    tx.send(AppEvent::OpenAccountLoginChoice { alias });
                } else {
                    tx.send(AppEvent::OpenAccountAliasPrompt {
                        value: Some(alias),
                        error: Some(alias_validation_message().to_string()),
                    });
                }
            }),
        ));
        self.request_redraw();
    }

    pub(crate) fn show_account_alias_editor(
        &mut self,
        profile: AccountProfile,
        value: Option<String>,
        error: Option<String>,
    ) {
        let tx = self.app_event_tx.clone();
        let initial = value.unwrap_or_else(|| profile.alias.clone());
        self.bottom_pane.show_text_prompt(CustomPromptView::new(
            format!("Edit {}", profile.alias),
            "Account alias".to_string(),
            initial,
            error,
            Box::new(move |alias| {
                if valid_account_alias(&alias) {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: AccountProfilesRequest::Update(AccountProfileUpdateParams {
                            account_id: profile.id.clone(),
                            alias: Some(alias),
                            enabled: None,
                            priority: None,
                            note: None,
                            clear_note: false,
                        }),
                    });
                } else {
                    tx.send(AppEvent::OpenAccountAliasEditor {
                        profile: profile.clone(),
                        value: Some(alias),
                        error: Some(alias_validation_message().to_string()),
                    });
                }
            }),
        ));
        self.request_redraw();
    }

    pub(crate) fn show_account_priority_editor(
        &mut self,
        profile: AccountProfile,
        value: Option<String>,
        error: Option<String>,
    ) {
        let tx = self.app_event_tx.clone();
        self.bottom_pane.show_text_prompt(CustomPromptView::new(
            format!("Priority for {}", profile.alias),
            "Higher first; smaller last (0–u32 max)".to_string(),
            value.unwrap_or_else(|| profile.priority.to_string()),
            error,
            Box::new(move |value| {
                if let Ok(priority) = value.trim().parse::<u32>() {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: AccountProfilesRequest::Update(AccountProfileUpdateParams {
                            account_id: profile.id.clone(),
                            alias: None,
                            enabled: None,
                            priority: Some(priority),
                            note: None,
                            clear_note: false,
                        }),
                    });
                } else {
                    tx.send(AppEvent::OpenAccountPriorityEditor {
                        profile: profile.clone(),
                        value: Some(value),
                        error: Some(priority_validation_message().to_string()),
                    });
                }
            }),
        ));
        self.request_redraw();
    }

    pub(crate) fn show_account_note_editor(
        &mut self,
        profile: AccountProfile,
        value: Option<String>,
        error: Option<String>,
    ) {
        let tx = self.app_event_tx.clone();
        let initial = value.unwrap_or_else(|| profile.note.clone().unwrap_or_default());
        self.bottom_pane.show_text_prompt(
            CustomPromptView::new(
                format!("Note for {}", profile.alias),
                "Account note".to_string(),
                initial,
                error,
                Box::new(move |note| {
                    if note.is_empty() {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Update(AccountProfileUpdateParams {
                                account_id: profile.id.clone(),
                                alias: None,
                                enabled: None,
                                priority: None,
                                note: None,
                                clear_note: true,
                            }),
                        });
                    } else if valid_account_note(&note) {
                        tx.send(AppEvent::OpenAccountProfiles {
                            request: AccountProfilesRequest::Update(AccountProfileUpdateParams {
                                account_id: profile.id.clone(),
                                alias: None,
                                enabled: None,
                                priority: None,
                                note: Some(note),
                                clear_note: false,
                            }),
                        });
                    } else {
                        tx.send(AppEvent::OpenAccountNoteEditor {
                            profile: profile.clone(),
                            value: Some(note),
                            error: Some(note_validation_message().to_string()),
                        });
                    }
                }),
            )
            .allow_empty_submit(),
        );
        self.request_redraw();
    }

    pub(super) fn show_account_error(
        &mut self,
        request: AccountProfilesRequest,
        error: AccountProfilesError,
    ) {
        if let AccountProfilesRequest::Update(params) = &request
            && let Some(profile) = self
                .account_profiles
                .iter()
                .find(|profile| profile.id == params.account_id)
                .cloned()
        {
            match error {
                AccountProfilesError::AliasInvalid | AccountProfilesError::AliasInUse
                    if params.alias.is_some() =>
                {
                    let message = if error == AccountProfilesError::AliasInUse {
                        "That alias is already in use.".to_string()
                    } else {
                        alias_validation_message().to_string()
                    };
                    self.show_account_alias_editor(profile, params.alias.clone(), Some(message));
                    return;
                }
                AccountProfilesError::NoteInvalid if params.note.is_some() || params.clear_note => {
                    self.show_account_note_editor(
                        profile,
                        params.note.clone(),
                        Some(note_validation_message().to_string()),
                    );
                    return;
                }
                AccountProfilesError::PriorityInvalid if params.priority.is_some() => {
                    self.show_account_priority_editor(
                        profile,
                        params.priority.map(|priority| priority.to_string()),
                        Some(priority_validation_message().to_string()),
                    );
                    return;
                }
                AccountProfilesError::AliasInvalid
                | AccountProfilesError::AliasInUse
                | AccountProfilesError::NoteInvalid
                | AccountProfilesError::PriorityInvalid
                | AccountProfilesError::Unavailable => {}
            }
        }
        if let AccountProfilesRequest::LoginStart { alias, .. } = &request
            && matches!(
                error,
                AccountProfilesError::AliasInvalid | AccountProfilesError::AliasInUse
            )
        {
            self.show_account_alias_prompt(
                Some(alias.clone()),
                Some(if error == AccountProfilesError::AliasInUse {
                    "That alias is already in use.".to_string()
                } else {
                    alias_validation_message().to_string()
                }),
            );
            return;
        }
        let retry = request.clone();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(ACCOUNT_VIEW_ID),
            title: Some(request.title().to_string()),
            subtitle: Some("The account request could not be completed.".to_string()),
            items: vec![SelectionItem {
                name: "Try again".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAccountProfiles {
                        request: retry.clone(),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            }],
            footer_hint: Some(standard_popup_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }
}

fn valid_account_alias(value: &str) -> bool {
    let mut characters = value.chars();
    (1..=64).contains(&value.len())
        && characters
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        })
}

fn valid_account_note(value: &str) -> bool {
    value.len() <= 1_024 && !value.chars().any(char::is_control)
}

fn alias_validation_message() -> &'static str {
    "Lowercase letters/digits/._-; start with one (1–64)."
}

fn priority_validation_message() -> &'static str {
    "Higher first; smaller last (0–u32 max)"
}

fn note_validation_message() -> &'static str {
    "Max 1024 bytes; no control characters."
}
