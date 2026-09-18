use crate::redact_secrets;
use crate::sanitizer::compile_regex;
use regex::Regex;
use std::sync::LazyLock;

static URL: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r#"(?i)\b(?:https?|wss?)://[^\s\"'<>]+"#));
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r#"(?i)\b(authorization|proxy-authorization|cookie|set-cookie)\s*[:=]\s*[^\r\n]+"#,
    )
});
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r#"(?i)\b(access[_-]?token|refresh[_-]?token|id[_-]?token|api[_-]?key|client[_-]?secret|device[_-]?code|password|secret|token|state)\b[\"']?\s*[:=]\s*[\"']?[^\s\"',;&]+"#,
    )
});

/// Sanitize free-text network diagnostics. Never use this to retain payloads or
/// arbitrary headers: callers must also allowlist the fields they record.
pub fn redact_network_diagnostic(input: &str) -> String {
    let safe = URL.replace_all(input, "[REDACTED_URL]");
    let safe = HEADER.replace_all(&safe, "$1: [REDACTED_SECRET]");
    let safe = ASSIGNMENT.replace_all(&safe, "$1=[REDACTED_SECRET]");
    redact_secrets(safe.into_owned())
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
