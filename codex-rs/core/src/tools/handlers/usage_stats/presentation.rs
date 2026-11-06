use super::*;
use codex_usage::OperationSelection;

/// Preserve a complete JSON envelope and a continuation for every omitted row.
pub(super) fn record_page(
    store: &UsageReader,
    range: TimeRange,
    filter: &UsageFilter,
    args: &UsageStatsArgs,
    limit: usize,
) -> Result<Value, FunctionCallError> {
    let detail = args.detail.as_deref().unwrap_or("tokens");
    let operation = match args.action {
        UsageStatsAction::Tools => Some(OperationSelection::Tools),
        UsageStatsAction::Activities => Some(OperationSelection::Classified),
        UsageStatsAction::Details => match detail {
            "operations" => Some(OperationSelection::All),
            "model_requests" => Some(OperationSelection::ModelRequests),
            "tools" => Some(OperationSelection::Tools),
            _ => None,
        },
        _ => None,
    };
    let cursor = match (args.cursor_sort_value, args.cursor_id.as_ref()) {
        (Some(value), Some(id)) => Some(RecordCursor::from_millis(value, id)),
        (None, None) => None,
        _ => {
            return Err(tool_error(
                "cursor_id and cursor_sort_value must be supplied together",
            ));
        }
    };
    let kind = match args.action {
        UsageStatsAction::Tools => "usageTools",
        UsageStatsAction::Activities => "usageActivities",
        UsageStatsAction::Events => "usageEvents",
        UsageStatsAction::Details => "usageDetails",
        _ => unreachable!("only list and detail actions call record_page"),
    };
    let (mut data, mut next, cursors, coverage) = if let Some(selection) = operation {
        let page = store
            .operation_page(range, filter, selection, cursor.as_ref(), limit)
            .map_err(|_| storage_error())?;
        let cursors = page
            .data
            .iter()
            .map(|row| {
                RecordCursor::from_millis(
                    row.started_at_ms.unwrap_or(row.observed_at_ms),
                    row.operation_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let data = page.data.iter().map(|row| json!({
            "id":row.operation_id,"operationId":row.operation_id,"operationKind":row.kind,
            "threadId":row.thread_id,"turnId":row.turn_id,"accountId":row.account_id,
            "repositoryId":row.repository_id,"toolName":row.tool,"model":row.model,"responseId":row.response_id,
            "startedAtMs":row.started_at_ms,"completedAtMs":row.ended_at_ms,"observedAtMs":row.observed_at_ms,
            "durationMs":row.duration_ms,"status":row.outcome,"phase":row.phase,"activity":row.activity,
            "provenance":row.classification,"coverage":row.coverage,"attemptNumber":row.attempt_number,
            "reworkOfOperationId":row.rework_of_operation_id,
        })).collect::<Vec<_>>();
        (data, page.next_cursor, cursors, "recorded_operations")
    } else {
        let (selection, coverage) = if matches!(args.action, UsageStatsAction::Details) {
            match detail {
                "tokens" => (RecordKind::Tokens, "recorded"),
                "threads" => (RecordKind::Threads, "recorded_lifecycle"),
                "turns" => (RecordKind::Turns, "recorded_lifecycle"),
                "classifications" => (RecordKind::Classifications, "recorded"),
                "coverage" => (RecordKind::Coverage, "recorded"),
                "activity_spans" => (RecordKind::Classifications, "recorded_declarations"),
                "lifecycle_events" | "events" => (RecordKind::Lifecycle, "recorded"),
                "repository_attributions" | "repository_identities" | "repository_events" => {
                    (RecordKind::Repositories, "unobserved")
                }
                "approvals" => (RecordKind::Approvals, "unobserved"),
                "processes" => (RecordKind::Processes, "unobserved"),
                "agents" => (RecordKind::Agents, "unobserved"),
                "taxonomies" => (RecordKind::Taxonomies, "unobserved"),
                _ => return Err(tool_error("unsupported usage detail family")),
            }
        } else {
            (RecordKind::All, "recorded")
        };
        let page = store
            .page(range, filter, selection, cursor.as_ref(), limit)
            .map_err(|_| storage_error())?;
        let cursors = page
            .records
            .iter()
            .map(|row| RecordCursor::from_millis(row.effective_observed_at_ms(), row.id.clone()))
            .collect::<Vec<_>>();
        (
            page.records.iter().map(record_fields).collect(),
            page.next,
            cursors,
            coverage,
        )
    };
    // Reserve room for metadata and the envelope. A single bounded runtime fact
    // fits; large result sets continue from the last row actually returned.
    while data.len() > 1
        && serde_json::to_vec(&data)
            .map_err(|_| storage_error())?
            .len()
            > MAX_OUTPUT_BYTES - 2048
    {
        data.pop();
        next = cursors.get(data.len() - 1).cloned();
    }
    let mut value = json!({"schemaVersion":1,"kind":kind,"data":data,"nextCursor":next.map(|cursor|json!({"sortValue":cursor.observed_at_ms,"id":cursor.id})),"coverage":coverage});
    if matches!(args.action, UsageStatsAction::Details) {
        value["detailKind"] = json!(detail);
    }
    Ok(value)
}
