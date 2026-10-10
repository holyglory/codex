use super::redact_network_diagnostic;
use pretty_assertions::assert_eq;

#[test]
fn network_errors_preserve_reason_but_remove_credentials_and_urls() {
    for (input, expected) in [
        (
            "policy violation; token=short",
            "policy violation; token=[REDACTED_SECRET]",
        ),
        (
            "refresh_token=oauth-value timeout",
            "refresh_token=[REDACTED_SECRET] timeout",
        ),
        (
            "Authorization: Basic c2VjcmV0",
            "Authorization: [REDACTED_SECRET]",
        ),
        ("cookie: session=private", "cookie: [REDACTED_SECRET]"),
        (
            r#"{"authorization":"Basic c2VjcmV0"}"#,
            r#"{"authorization: [REDACTED_SECRET]"#,
        ),
        (
            "connection refused https://user:pass@host/path?state=private",
            "connection refused [REDACTED_URL]",
        ),
        ("TLS certificate expired", "TLS certificate expired"),
        ("policy violation (1008)", "policy violation (1008)"),
    ] {
        assert_eq!(redact_network_diagnostic(input), expected);
    }
}
