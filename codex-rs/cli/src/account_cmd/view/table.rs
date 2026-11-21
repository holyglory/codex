use std::io::IsTerminal;

use owo_colors::OwoColorize;
use owo_colors::Style;
use unicode_width::UnicodeWidthStr;

use super::AccountListEntry;
use super::compare_human_accounts;
use super::limits;
use super::safe_human_text;

pub(super) fn print(mut entries: Vec<AccountListEntry<'_>>) {
    if entries.is_empty() {
        println!("No account profiles configured.");
        return;
    }
    let has_limits = entries.iter().any(|entry| entry.limits.is_some());
    // `account priority list` retains its documented higher-first order.
    if has_limits {
        entries.sort_by(compare_human_accounts);
    }
    let has_notes = entries.iter().any(|entry| {
        entry
            .account
            .note
            .as_deref()
            .is_some_and(|note| !note.trim().is_empty())
    });
    let color = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && supports_color::on(supports_color::Stream::Stdout).is_some();
    let now = chrono::Utc::now().timestamp();
    let plain = Style::new();
    let highlight = Style::new().bold().green();
    let mut headers = vec!["ALIAS", "PRIORITY"];
    if has_notes {
        headers.push("NOTE");
    }
    if has_limits {
        headers.extend([
            "CREDITS",
            "CREDIT USE",
            "BANKED RESETS",
            "LIMITS",
            "RESET IN",
        ]);
    }
    let mut rows = vec![
        headers
            .iter()
            .map(|text| (text.to_string(), Style::new().bold().cyan()))
            .collect::<Vec<_>>(),
    ];
    for entry in entries {
        let account = entry.account;
        let mut row = vec![
            (
                format!(
                    "{}{}",
                    if account.current { "*" } else { " " },
                    account.alias
                ),
                if account.current { highlight } else { plain },
            ),
            (account.priority.to_string(), plain),
        ];
        if has_notes {
            row.push((
                account
                    .note
                    .as_deref()
                    .map(safe_human_text)
                    .unwrap_or_default(),
                plain,
            ));
        }
        if has_limits {
            row.push((super::credits::label(entry.limits), plain));
            row.push((
                (if account.credit_usage_enabled {
                    "enabled"
                } else {
                    "disabled"
                })
                .to_string(),
                plain,
            ));
            if let Some(limits) = entry.limits {
                let banked = match &limits.banked_resets {
                    Some(resets) if resets.available_count > 0 => {
                        let expiry = if !resets.expiry_known {
                            "unknown".to_string()
                        } else if let Some(expires_at) = resets.soonest_expires_at {
                            limits::reset_countdown_days_hours(Some(expires_at), now)
                        } else {
                            "never".to_string()
                        };
                        (format!("{} ({expiry})", resets.available_count), highlight)
                    }
                    Some(resets) if resets.available_count == 0 => ("none".to_string(), plain),
                    Some(resets) => (resets.available_count.to_string(), plain),
                    None => ("unknown".to_string(), plain),
                };
                row.push(banked);
                row.push(if limits.state == "observed" {
                    match limits::codex_usage_percent(limits) {
                        Some(percent) => {
                            let style = if percent >= 67.0 {
                                Style::new().red()
                            } else if percent >= 34.0 {
                                Style::new().yellow()
                            } else {
                                Style::new().green()
                            };
                            (format!("{percent:.0}%"), style)
                        }
                        None => ("unknown".to_string(), plain),
                    }
                } else {
                    (
                        format!("unknown ({})", limits.reason.unwrap_or("unavailable")),
                        plain,
                    )
                });
                let reset_style = match limits.next_reset_at {
                    Some(reset) if reset.saturating_sub(now) < 86_400 => Style::new().green(),
                    Some(reset) if reset.saturating_sub(now) < 6 * 86_400 => Style::new().yellow(),
                    Some(_) => Style::new().red(),
                    None => plain,
                };
                row.push((
                    limits::reset_countdown(limits.next_reset_at, now),
                    reset_style,
                ));
            } else {
                row.extend(std::iter::repeat_n(("unknown".to_string(), plain), 3));
            }
        }
        rows.push(row);
    }
    // Measure display cells before adding ANSI escapes so wide text stays aligned.
    let mut widths = vec![0; headers.len()];
    for row in &rows {
        for (width, (text, _)) in widths.iter_mut().zip(row) {
            *width = (*width).max(text.width());
        }
    }
    for row in rows {
        let last = row.len() - 1;
        let mut line = String::new();
        for (index, (text, style)) in row.into_iter().enumerate() {
            let text = if index == last {
                text.trim_end().to_string()
            } else {
                text
            };
            if color {
                line.push_str(&text.style(style).to_string());
            } else {
                line.push_str(&text);
            }
            if index < last {
                line.push_str(&" ".repeat(widths[index] - text.width() + 2));
            }
        }
        println!("{}", line.trim_end());
    }
}
