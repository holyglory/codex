use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn account_management_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("list"),
                    json!("set_priority"),
                    json!("set_all_priorities"),
                    json!("rename"), json!("enable"), json!("disable"), json!("set_default"), json!("set_auto_selection"),
                ],
                Some("Inspect accounts and limits, or manage existing profile metadata and routing.".to_string()),
            ),
        ),
        (
            "account".to_string(),
            JsonSchema::string(Some(
                "Account alias or local profile ID; required for profile-specific mutations.".to_string(),
            )),
        ),
        (
            "priority".to_string(),
            JsonSchema::integer(Some(
                "Unsigned priority; higher numbers drain first and smaller numbers drain last."
                    .to_string(),
            )),
        ),
        (
            "expected_generation".to_string(),
            JsonSchema::integer(Some(
                "Registry generation from list; required for rename, enable, disable, set_default and set_auto_selection.".to_string(),
            )),
        ),
        (
            "new_alias".to_string(), JsonSchema::string(Some("New profile alias for rename.".to_string())),
        ),
        (
            "mode".to_string(), JsonSchema::string_enum(vec![json!("enabled"), json!("disabled")], Some("Automatic-selection state for set_auto_selection.".to_string())),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some("List offset; defaults to 0.".to_string())),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some(
                "List page size; maximum 25, or 10 with service usage refresh.".to_string(),
            )),
        ),
        (
            "refresh_service_usage".to_string(),
            JsonSchema::boolean(Some(
                "For list only, fetch fresh bounded rate-limit usage for eligible managed ChatGPT profiles."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "account_management".to_string(),
        description: "Manage existing local account profiles and inspect current service limits. Use list before mutations and supply its expected_generation. Rename, enable/disable, choose the default profile, configure automatic selection, or change priorities. Changes preserve the current turn's credential lease and apply to subsequent routing. With refresh_service_usage, list returns live limit windows, Unix reset timestamps and the next main Codex reset in UTC with its window scope. Exhausted windows take priority over used or unused windows; auxiliary model quotas never determine that reset. Paginate to inspect every account. This tool never returns credentials, email, service/workspace identifiers or notes, and does not add profiles, perform login or remove profiles. Background account probes remain limited to managed ChatGPT profiles."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
