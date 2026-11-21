//! Preserve the provider metadata debug projection without host-only request state.
use super::CodexResponsesMetadata;

impl std::fmt::Debug for CodexResponsesMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexResponsesMetadata")
            .field("guardian_recorded_items", &self.guardian_recorded_items)
            .field("parent_response_id", &self.parent_response_id)
            .field("parent_routing_affinity", &self.parent_routing_affinity)
            .field("installation_id", &self.installation_id)
            .field("session_id", &self.session_id)
            .field("thread_id", &self.thread_id)
            .field("agent_name", &self.agent_name)
            .field("turn_id", &self.turn_id)
            .field("routing_hint", &self.routing_hint)
            .field("window_id", &self.window_id)
            .field("window_number", &self.window_number)
            .field("context_window_id", &self.context_window_id)
            .field("history_initialization", &self.history_initialization)
            .field("request_kind", &self.request_kind)
            .field("forked_from_thread_id", &self.forked_from_thread_id)
            .field(
                "forked_from_ordinal_exclusive",
                &self.forked_from_ordinal_exclusive,
            )
            .field("parent_thread_id", &self.parent_thread_id)
            .field("parent_turn_id", &self.parent_turn_id)
            .field("root_turn_id", &self.root_turn_id)
            .field("subagent_header", &self.subagent_header)
            .field("subagent_kind", &self.subagent_kind)
            .field("thread_source", &self.thread_source)
            .field("turn_trigger", &self.turn_trigger)
            .field("sandbox", &self.sandbox)
            .field("sandbox_mode", &self.sandbox_mode)
            .field("auto_review_enabled", &self.auto_review_enabled)
            .field(
                "node_repl_auto_review_required",
                &self.node_repl_auto_review_required,
            )
            .field("node_repl_disabled", &self.node_repl_disabled)
            .field("workspaces", &self.workspaces)
            .field("tool_namespaces_info", &self.tool_namespaces_info)
            .field("turn_started_at_unix_ms", &self.turn_started_at_unix_ms)
            .field("history_ingest_requested", &self.history_ingest_requested)
            .field("analytics_enabled", &self.analytics_enabled)
            .field("mcp_attribution", &self.mcp_attribution)
            .field("extra", &self.extra)
            .finish()
    }
}
