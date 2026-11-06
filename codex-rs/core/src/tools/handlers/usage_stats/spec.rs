use super::*;

pub(super) fn usage_stats_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("summary"),
                    json!("task_tree_summary"),
                    json!("performance_review"),
                    json!("repositories"),
                    json!("tools"),
                    json!("activities"),
                    json!("events"),
                    json!("details"),
                ],
                Some("Report or paged detail to read.".to_string()),
            ),
        ),
        (
            "scope".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("all"),
                    json!("current_chat"),
                    json!("current_repository"),
                    json!("repository"),
                ],
                Some("For summary only; defaults to current_chat.".to_string()),
            ),
        ),
        (
            "repository".to_string(),
            JsonSchema::string(Some(
                "Repository id, unique safe label, or current.".to_string(),
            )),
        ),
        (
            "account".to_string(),
            JsonSchema::string(Some(
                "Safe local account alias or local account reference for summary filtering."
                    .to_string(),
            )),
        ),
        (
            "thread_id".to_string(),
            JsonSchema::string(Some(
                "Thread id or current for details or performance_review.".to_string(),
            )),
        ),
        (
            "root_thread_id".to_string(),
            JsonSchema::string(Some(
                "Root task for task_tree_summary; defaults to current.".to_string(),
            )),
        ),
        (
            "include_descendants".to_string(),
            JsonSchema::boolean(Some(
                "Include recorded descendants: task_tree_summary defaults to true; thread-scoped performance_review defaults to false. Set true for daily task reviews that include delegated work."
                    .to_string(),
            )),
        ),
        (
            "outcome_cursor".to_string(),
            JsonSchema::string(Some("For performance_review, continue its fixed outcome snapshot; expired cursors require a new first page.".to_string())),
        ),
        (
            "outcome_limit".to_string(),
            JsonSchema::number(Some("For performance_review, outcome rows per page, 1–50 (default 8), subject to the response byte limit.".to_string())),
        ),
        (
            "agent_id".to_string(),
            JsonSchema::string(Some("Agent id for activity filtering.".to_string())),
        ),
        (
            "detail".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("processes"), json!("threads"), json!("turns"), json!("agents"),
                    json!("operations"), json!("tokens"), json!("approvals"),
                    json!("repository_attributions"), json!("classifications"), json!("coverage"),
                    json!("activity_spans"), json!("lifecycle_events"), json!("repository_identities"),
                    json!("repository_events"), json!("taxonomies"), json!("model_requests"), json!("tools"), json!("events"),
                ],
                Some("Record family for details.".to_string()),
            ),
        ),
        (
            "from_at_ms".to_string(),
            JsonSchema::number(Some("Inclusive UTC Unix milliseconds.".to_string())),
        ),
        (
            "to_at_ms".to_string(),
            JsonSchema::number(Some("Exclusive UTC Unix milliseconds.".to_string())),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some(
                "Detail/summary page size, 1–50. Summary totals repeat across pages.".to_string(),
            )),
        ),
        (
            "cursor_sort_value".to_string(),
            JsonSchema::number(Some("Sort value from the returned nextCursor.".to_string())),
        ),
        (
            "cursor_id".to_string(),
            JsonSchema::string(Some("Identifier from the returned nextCursor.".to_string())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description: "Read the private, content-free local usage collector. Summaries retain totals and paginate detail when needed; task_tree_summary includes recorded descendants. Details paginate every approved entity, model/tool attempt, token observation, approval, attribution, classification, coverage, wait, lifecycle, repository-evidence, and taxonomy field while omitting OS PIDs. This tool never returns prompts, output, source, commands, payloads, credentials, email, raw paths/remotes, or service/workspace identifiers. Use usage_activity correct_classification for an append-only enum correction."
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
