use std::cmp::Ordering;
use std::io::IsTerminal;

use owo_colors::OwoColorize;
use owo_colors::Style;
use unicode_width::UnicodeWidthStr;

use super::AccountListEntry;
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
        entries.sort_by(compare_accounts);
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
    // Measure plain display cells before adding ANSI escapes; tabs and string byte
    // lengths cannot align empty notes, wide Unicode text, and colored values.
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

fn compare_accounts(left: &AccountListEntry<'_>, right: &AccountListEntry<'_>) -> Ordering {
    let left_usage = left.limits.and_then(limits::codex_usage_percent);
    let right_usage = right.limits.and_then(limits::codex_usage_percent);
    let left_group = usage_group(left_usage);
    let right_group = usage_group(right_usage);
    left.account
        .priority
        .cmp(&right.account.priority)
        .then_with(|| left_group.cmp(&right_group))
        .then_with(|| {
            if left_group == 0 {
                left_usage
                    .unwrap_or(100.0)
                    .total_cmp(&right_usage.unwrap_or(100.0))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| {
            if left_group == 1 {
                banked_count(right).cmp(&banked_count(left))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| reset_at(left).cmp(&reset_at(right)))
        .then_with(|| left.account.alias.cmp(&right.account.alias))
        .then_with(|| left.account.id.cmp(&right.account.id))
}

fn usage_group(usage: Option<f64>) -> u8 {
    match usage {
        Some(percent) if percent < 100.0 => 0,
        Some(_) => 1,
        None => 2,
    }
}

fn banked_count(entry: &AccountListEntry<'_>) -> i64 {
    entry
        .limits
        .and_then(|limits| limits.banked_resets.as_ref())
        .map_or(-1, |resets| resets.available_count)
}

fn reset_at(entry: &AccountListEntry<'_>) -> i64 {
    entry
        .limits
        .and_then(|limits| limits.next_reset_at)
        .unwrap_or(i64::MAX)
}
