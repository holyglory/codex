use codex_protocol::protocol::RateLimitSnapshot;

/// The next reset of a relevant main Codex quota window.
#[derive(Debug, PartialEq, Eq)]
pub struct CodexLimitReset {
    pub resets_at: i64,
    pub scope: &'static str,
}

/// Prefer exhausted windows, then used windows, then unused windows. Auxiliary
/// model buckets cannot supply a reset for the main Codex quota. Missing reset
/// evidence in the preferred class must not fall back to a healthier window.
pub fn next_codex_limit_reset(
    snapshots: &[RateLimitSnapshot],
    observed_at: i64,
) -> Option<CodexLimitReset> {
    let codex = snapshots
        .iter()
        .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))?;
    let windows = [
        codex
            .primary
            .as_ref()
            .map(|window| (window.used_percent, window.resets_at, "codex.primary")),
        codex
            .secondary
            .as_ref()
            .map(|window| (window.used_percent, window.resets_at, "codex.secondary")),
        codex.individual_limit.as_ref().map(|limit| {
            (
                100.0 - f64::from(limit.remaining_percent),
                Some(limit.resets_at),
                "codex.individual",
            )
        }),
    ];
    let priority = |used: f64| {
        if used >= 100.0 {
            2
        } else {
            i32::from(used > 0.0)
        }
    };
    let preferred = windows
        .iter()
        .flatten()
        .map(|(used, _, _)| priority(*used))
        .max()?;
    windows
        .into_iter()
        .flatten()
        .filter(|(used, _, _)| priority(*used) == preferred)
        .filter_map(|(_, resets_at, scope)| {
            Some(CodexLimitReset {
                resets_at: resets_at?,
                scope,
            })
        })
        .filter(|reset| reset.resets_at > observed_at)
        .min_by_key(|reset| reset.resets_at)
}

#[cfg(test)]
#[path = "limit_reset_tests.rs"]
mod tests;
