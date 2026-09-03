//! Test-only helpers shared across the TUI crate.

use std::sync::LazyLock;

use codex_models_manager::bundled_models_response;
use codex_protocol::openai_models::ModelPreset;
pub(crate) use codex_utils_absolute_path::test_support::PathBufExt;
pub(crate) use codex_utils_absolute_path::test_support::test_path_buf;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) static TEST_MODEL_PRESETS: LazyLock<Vec<ModelPreset>> = LazyLock::new(|| {
    let mut response = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    response.models.sort_by_key(|model| model.priority);
    let mut presets: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
    ModelPreset::mark_default_by_picker_visibility(&mut presets);
    presets
});

const NORMALIZED_CLI_VERSION_LABEL: &str = "(v<VERSION>)";

pub(crate) fn normalize_cli_version(rendered: String) -> String {
    normalize_cli_version_label(
        rendered,
        crate::version::CODEX_CLI_VERSION,
        NORMALIZED_CLI_VERSION_LABEL,
    )
}

fn normalize_cli_version_for(rendered: String, cli_version: &str) -> String {
    normalize_cli_version_label(rendered, cli_version, NORMALIZED_CLI_VERSION_LABEL)
}

pub(crate) fn normalize_cli_version_to(rendered: String, normalized_version: &str) -> String {
    normalize_cli_version_label(
        rendered,
        crate::version::CODEX_CLI_VERSION,
        &format!("(v{normalized_version})"),
    )
}

fn normalize_cli_version_label(
    rendered: String,
    cli_version: &str,
    normalized_label: &str,
) -> String {
    let runtime_label = format!("(v{cli_version})");
    let field_width = runtime_label.len().max(normalized_label.len());
    // Replace equal-width fields so short Bazel versions borrow right padding while long
    // downstream versions donate it, leaving surrounding snapshot geometry unchanged.
    let runtime_field = format!("{runtime_label:<field_width$}");
    let normalized_field = format!("{normalized_label:<field_width$}");
    rendered
        .replace(&runtime_field, &normalized_field)
        .lines()
        .map(|line| {
            let Some(start) = line.find("(v") else {
                return line.to_string();
            };
            let border = line.rfind('│').unwrap_or(line.len());
            if start >= border {
                return line.to_string();
            }
            let visible_end = start + line[start..border].trim_end().len();
            let visible = &line[start..visible_end];
            if !runtime_label.starts_with(visible) {
                return line.to_string();
            }
            let mut replacement = normalized_label
                .chars()
                .take(visible.len())
                .collect::<String>();
            replacement.push_str(&" ".repeat(visible.len().saturating_sub(replacement.len())));
            let mut normalized = line.to_string();
            normalized.replace_range(start..visible_end, &replacement);
            normalized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn test_path_display(path: &str) -> String {
    test_path_buf(path).display().to_string()
}

pub(crate) fn session_source_cli<T>() -> T
where
    T: DeserializeOwned,
{
    from_app_server_wire(codex_app_server_protocol::SessionSource::Cli)
}

pub(crate) fn skill_scope_user<T>() -> T
where
    T: DeserializeOwned,
{
    from_app_server_wire(codex_app_server_protocol::SkillScope::User)
}

pub(crate) fn skill_scope_repo<T>() -> T
where
    T: DeserializeOwned,
{
    from_app_server_wire(codex_app_server_protocol::SkillScope::Repo)
}

fn from_app_server_wire<T>(value: impl Serialize) -> T
where
    T: DeserializeOwned,
{
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .unwrap_or_else(|err| {
            panic!("app-server wire value should map to legacy helper type: {err}")
        })
}

#[cfg(test)]
#[path = "test_support_tests.rs"]
mod tests;
