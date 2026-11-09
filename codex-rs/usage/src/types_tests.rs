use super::*;

#[test]
fn field_specific_identifiers_reject_content_shapes_and_redact_debug() {
    for invalid in [
        "/home/user/repo",
        "https://example.com",
        "parent/child",
        "parent\\child",
        "..",
        "a..b",
        "line\nbreak",
    ] {
        assert!(ThreadId::new(invalid).is_err());
        assert!(ProviderKind::new(invalid).is_err());
        assert!(ModelName::new(invalid).is_err());
        assert!(ToolName::new(invalid).is_err());
        assert!(CoverageReasonCode::new(invalid).is_err());
    }
    let thread = ThreadId::new("thread-123").expect("thread id");
    assert_eq!(format!("{thread:?}"), "ThreadId([redacted])");
}

#[test]
fn tool_request_shape_keeps_only_field_names_and_types() {
    let shape = ToolRequestShape::from_json_object(
        serde_json::json!({
            "command": "string",
            "credential": "string"
        })
        .as_object()
        .expect("object"),
    )
    .expect("bounded request shape");
    assert!(shape.as_str().contains("command"));
    assert!(!shape.as_str().contains("secret-value"));
    assert_eq!(format!("{shape:?}"), "ToolRequestShape([redacted])");
    assert!(ToolRequestShape::new(r#"{"command":"secret-value"}"#).is_err());
}
