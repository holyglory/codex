use super::*;

impl AccountProfilesRequest {
    pub(super) fn title(&self) -> &'static str {
        match self {
            Self::List { .. } => "Accounts",
            Self::Activate { .. } => "Selecting account",
            Self::Update(_) => "Updating account",
            Self::Remove { .. } => "Removing account",
            Self::Limits { .. } => "Account limits",
            Self::AutoRead | Self::AutoWrite { .. } => "Automatic account selection",
            Self::LoginStart { .. } | Self::LoginCancel { .. } => "Account sign-in",
        }
    }
}

pub(super) fn resolve_profile<'a>(
    profiles: &'a [AccountProfile],
    reference: &str,
) -> Option<&'a AccountProfile> {
    let mut matches = profiles
        .iter()
        .filter(|profile| profile.id == reference || profile.alias == reference);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

pub(super) fn account_item(
    profile: &AccountProfile,
    limits: Option<&AccountProfileRateLimitReadResponse>,
) -> SelectionItem {
    let selected = profile.clone();
    SelectionItem {
        name: profile.alias.clone(),
        name_prefix_spans: profile
            .is_active
            .then(|| "● ".green())
            .into_iter()
            .collect(),
        description: Some(profile_description(profile, limits)),
        is_current: profile.is_active,
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::ShowAccountProfileActions {
                profile: selected.clone(),
            })
        })],
        dismiss_on_select: false,
        search_value: profile.email.clone(),
        ..Default::default()
    }
}

pub(super) fn profile_description(
    profile: &AccountProfile,
    limits: Option<&AccountProfileRateLimitReadResponse>,
) -> String {
    let identity = profile
        .email
        .clone()
        .unwrap_or_else(|| "email unavailable".to_string());
    let plan = profile
        .plan_type
        .map(|plan| format!("{plan:?}"))
        .unwrap_or_else(|| "plan unknown".to_string());
    let authentication = if profile.authenticated {
        "signed in"
    } else {
        "logged out"
    };
    let state = if profile.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let default = if profile.is_default {
        " · default"
    } else {
        ""
    };
    let limit = limits.map_or_else(
        || "limits unknown".to_string(),
        |limits| {
            if limits.data.is_empty() {
                "limits unknown".to_string()
            } else {
                limits
                    .data
                    .iter()
                    .map(rate_limit_summary)
                    .collect::<Vec<_>>()
                    .join("; ")
            }
        },
    );
    format!(
        "{identity} · {plan} · {authentication} · {state}{default} · priority {} · {limit}",
        profile.priority
    )
}

pub(super) fn rate_limit_summary(snapshot: &RateLimitSnapshot) -> String {
    let primary = snapshot
        .primary
        .as_ref()
        .map_or("primary unknown".to_string(), |window| {
            format!(
                "primary {}% reset {}",
                window.used_percent,
                window
                    .resets_at
                    .map_or("unknown".to_string(), |value| value.to_string())
            )
        });
    let secondary = snapshot
        .secondary
        .as_ref()
        .map_or("secondary unknown".to_string(), |window| {
            format!(
                "secondary {}% reset {}",
                window.used_percent,
                window
                    .resets_at
                    .map_or("unknown".to_string(), |value| value.to_string())
            )
        });
    format!("{primary}, {secondary}")
}

pub(super) fn disabled_item(name: String) -> SelectionItem {
    SelectionItem {
        name,
        is_disabled: true,
        ..Default::default()
    }
}
