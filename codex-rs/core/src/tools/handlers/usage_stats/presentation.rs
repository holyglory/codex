use super::*;
use codex_usage::OperationSelection;

/// Reduce only the new operation facets, retaining every count and interval
/// in an explicit remainder. Older report sections keep their existing bound.
pub(super) fn coalesce_operation_facet(value: &mut Value) -> Result<bool, FunctionCallError> {
    if value["kind"].as_str() != Some("performanceReview") {
        return Ok(false);
    }
    let candidate = ["operationsByActivity", "tools"]
        .into_iter()
        .filter_map(|key| {
            let rows = value[key].as_array()?;
            let index = rows.iter().rposition(|row| row["remainder"] != true)?;
            Some((key, index, rows[index].to_string().len()))
        })
        .max_by_key(|(_, _, bytes)| *bytes);
    let Some((key, index, _)) = candidate else {
        return Ok(false);
    };
    let rows = value[key].as_array_mut().ok_or_else(storage_error)?;
    let row = rows.remove(index);
    let mut remainder = if let Some(index) = rows.iter().position(|row| row["remainder"] == true) {
        rows.remove(index)
    } else {
        json!({"count":0,"measuredIntervalSumMs":0,"unknownIntervals":0,"mergedGroups":0,"remainder":true})
    };
    for metric in [
        "count",
        "measuredIntervalSumMs",
        "unknownIntervals",
        "mergedGroups",
    ] {
        let sum = remainder[metric]
            .as_u64()
            .ok_or_else(storage_error)?
            .checked_add(row[metric].as_u64().ok_or_else(storage_error)?)
            .ok_or_else(storage_error)?;
        remainder[metric] = json!(sum);
    }
    let object = remainder.as_object_mut().ok_or_else(storage_error)?;
    for dimension in ["phase", "activity", "provenance", "tool", "outcome"] {
        object.remove(dimension);
    }
    rows.push(remainder);
    Ok(true)
}

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
            "processId":row.process_id,"agentId":row.agent_id,"retryOfOperationId":row.retry_of_operation_id,
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
                "activity_spans" => (RecordKind::ActivitySpans, "recorded_observations"),
                "lifecycle_events" | "events" => (RecordKind::Lifecycle, "recorded"),
                "repository_attributions" => {
                    (RecordKind::RepositoryAttributions, "recorded_observations")
                }
                "repository_identities" => {
                    (RecordKind::RepositoryIdentities, "recorded_observations")
                }
                "repository_events" => (RecordKind::RepositoryEvents, "recorded_observations"),
                "approvals" => (RecordKind::Approvals, "recorded_observations"),
                "processes" => (RecordKind::Processes, "recorded_observations"),
                "agents" => (RecordKind::Agents, "recorded_observations"),
                "taxonomies" => (RecordKind::Taxonomies, "recorded_observations"),
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
            page.records
                .iter()
                .map(|record| {
                    if matches!(args.action, UsageStatsAction::Details) {
                        entity_fields(detail, record)
                    } else {
                        record_fields(record)
                    }
                })
                .collect(),
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

/// Agent identity is the runtime thread session UUID. Intersect the indexed
/// thread restriction after scope/tree planning; never replace other filters.
pub(super) fn agent_filter(
    mut filter: UsageFilter,
    agent_id: Option<&str>,
) -> Result<UsageFilter, FunctionCallError> {
    if let Some(agent_id) = agent_id {
        let agent = uuid::Uuid::parse_str(agent_id)
            .map_err(|_| tool_error("agent_id must be a runtime thread UUID"))?
            .to_string();
        let threads = filter
            .thread_ids
            .take()
            .unwrap_or_else(|| [agent.clone()].into_iter().collect());
        filter.thread_ids = Some(
            threads
                .into_iter()
                .filter(|thread| thread == &agent)
                .collect(),
        );
    }
    Ok(filter)
}

fn entity_fields(detail: &str, record: &codex_usage::UsageRecord) -> Value {
    match detail {
        "processes" => json!({"type":"process", "id":record.process_id,
            "firstObservedAtMs":record.effective_observed_at_ms(),
            "startedAtMs":record.effective_started_at_ms(), "endedAtMs":record.effective_ended_at_ms()}),
        "agents" => json!({"type":"agent", "id":record.agent_id, "agentId":record.agent_id,
            "identityKind":"thread_session", "incarnationId":record.operation_id, "threadId":record.thread_id, "parentAgentId":record.parent_thread_id,
            "processId":record.process_id, "event":record.event,
            "startedAtMs":record.effective_started_at_ms(), "endedAtMs":record.effective_ended_at_ms()}),
        "activity_spans" => json!({"type":"activity_span", "id":record.operation_id,
            "agentId":record.agent_id, "processId":record.process_id, "threadId":record.thread_id,
            "phase":record.phase, "activity":record.activity, "provenance":record.classification,
            "reworkOfOperationId":record.rework_of_operation_id,
            "startedAtMs":record.effective_started_at_ms(), "endedAtMs":record.effective_ended_at_ms(),
            "heartbeatCount":record.heartbeat_count}),
        "approvals" => {
            json!({"type":"approval", "eventId":record.id, "operationId":record.operation_id,
            "agentId":record.agent_id, "processId":record.process_id, "toolName":record.tool,
            "event":record.event, "decision":record.approval_decision, "source":record.approval_source,
            "coverage":record.coverage, "requestedAtMs":record.effective_started_at_ms(),
            "resolvedAtMs":record.effective_ended_at_ms(), "reviewDurationMs":record.duration_ms})
        }
        "repository_attributions" => json!({"type":"repository_attribution", "eventId":record.id,
            "operationId":record.operation_id, "repositoryId":record.repository_id,
            "agentId":record.agent_id, "processId":record.process_id, "threadId":record.thread_id,
            "provenance":record.classification, "coverage":record.coverage, "observedAtMs":record.effective_observed_at_ms()}),
        "repository_identities" => json!({"type":"repository_identity", "id":record.repository_id,
            "safeDisplayLabel":record.repository_id, "identitySource":"validated_git_common_directory",
            "observedAtMs":record.effective_observed_at_ms()}),
        "repository_events" => json!({"type":"repository_event", "eventId":record.id,
            "repositoryId":record.repository_id, "agentId":record.agent_id, "processId":record.process_id,
            "event":record.event, "observedAtMs":record.effective_observed_at_ms()}),
        "taxonomies" => json!({"type":"taxonomy", "version":record.taxonomy_version,
            "mappingKey":record.classification, "processId":record.process_id,
            "observedAtMs":record.effective_observed_at_ms()}),
        _ => record_fields(record),
    }
}
