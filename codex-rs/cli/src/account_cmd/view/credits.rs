use super::super::limits::AccountLimitsJson;

pub(super) fn label(limits: Option<&AccountLimitsJson>) -> String {
    let credits = limits
        .filter(|limits| limits.state == "observed")
        .and_then(|limits| {
            limits
                .buckets
                .iter()
                .find(|bucket| bucket.limit_id.as_deref() == Some("codex"))
        })
        .and_then(|bucket| bucket.credits.as_ref());
    match credits {
        Some(credits) if credits.unlimited => "unlimited".to_string(),
        Some(credits) if !credits.has_credits => "0".to_string(),
        Some(credits) => credits
            .balance
            .as_deref()
            .filter(|balance| {
                !balance.trim().is_empty()
                    && balance.len() <= 128
                    && !balance.chars().any(char::is_control)
            })
            .unwrap_or("available")
            .to_string(),
        None => "unknown".to_string(),
    }
}
