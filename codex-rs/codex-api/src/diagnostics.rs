/// Provider error identifiers are useful diagnostics; free-text messages and
/// response bodies can contain conversation content and must not be retained.
pub(crate) fn error_code(value: &str) -> Option<&str> {
    (value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    .then_some(value)
}
