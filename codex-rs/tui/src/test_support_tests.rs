use super::normalize_cli_version_for;
use pretty_assertions::assert_eq;

#[test]
fn normalizes_cli_versions_without_changing_the_rendered_width() {
    let cases = [
        (
            "│ OpenAI Codex (v0.0.0)            │",
            "0.0.0",
            "│ OpenAI Codex (v<VERSION>)        │",
        ),
        (
            "│ OpenAI Codex (v0.153.0-alpha.6+multi.4)  │",
            "0.153.0-alpha.6+multi.4",
            "│ OpenAI Codex (v<VERSION>)                │",
        ),
        (
            "unrelated rendered line",
            "0.0.0",
            "unrelated rendered line",
        ),
    ];

    let actual = cases.map(|(rendered, cli_version, _expected)| {
        normalize_cli_version_for(rendered.to_string(), cli_version)
    });
    let expected = cases.map(|(_rendered, _cli_version, expected)| expected.to_string());

    assert_eq!(actual, expected);
}
