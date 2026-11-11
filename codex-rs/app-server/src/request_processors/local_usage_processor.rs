//! Read-only adapter over the producer's pinned usage-v2 reader. Never open the
//! retired SQLite store or use request content to infer work attribution.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LocalUsageCursor;
use codex_app_server_protocol::LocalUsageRepositoryIdentityParams;
use codex_app_server_protocol::LocalUsageRepositoryIdentityResponse;
use codex_app_server_protocol::LocalUsageSnapshot;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_usage::Freshness;
use codex_usage::OperationSelection;
use codex_usage::RecordCursor;
use codex_usage::RecordKind;
use codex_usage::ReviewOperationKind;
use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use serde_json::Value;
use serde_json::json;

use super::local_usage_bindings::Bindings;
use super::local_usage_bindings::safe_identifier;
use super::local_usage_snapshots;
use super::local_usage_snapshots::ViewError;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub(crate) async fn summary(
    codex_home: PathBuf,
    params: LocalUsageSummaryParams,
) -> Result<LocalUsageSummaryResponse, JSONRPCErrorError> {
    validate(&params)?;
    tokio::task::spawn_blocking(move || read(codex_home, params))
        .await
        .map_err(|_| internal_error("local usage reader unavailable"))?
}

pub(crate) async fn repository_identity(
    params: LocalUsageRepositoryIdentityParams,
) -> Result<LocalUsageRepositoryIdentityResponse, JSONRPCErrorError> {
    if params.contract_version.unwrap_or(1) != 1 {
        return Err(invalid_params("unsupported local usage contract version"));
    }
    let path = PathBuf::from(params.path);
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(invalid_params(
            "repository identity requires an absolute local path",
        ));
    }
    tokio::task::spawn_blocking(move || LocalUsageRepositoryIdentityResponse {
        contract_version: 1,
        source: "usage-v2".into(),
        repository_id: codex_core::usage_repository_id(&path),
        format_version: codex_core::USAGE_REPOSITORY_ID_FORMAT_VERSION,
    })
    .await
    .map_err(|_| internal_error("local repository identity unavailable"))
}

fn validate(params: &LocalUsageSummaryParams) -> Result<(), JSONRPCErrorError> {
    if params.contract_version.unwrap_or(1) != 1 {
        return Err(invalid_params("unsupported local usage contract version"));
    }
    if params.worktree_keys.is_some() {
        return Err(invalid_params(
            "worktree selection is not captured by usage-v2",
        ));
    }
    if params
        .workstream_id
        .as_deref()
        .is_some_and(|value| !safe_identifier(value, /*experiment*/ false))
    {
        return Err(invalid_params("invalid workstream identifier"));
    }
    Bindings::merge_request(
        /*sidecar*/ None,
        params.work_context_bindings.as_deref(),
    )
    .map_err(|_| invalid_params("invalid or conflicting work context declarations"))?;
    if (params.include_review || params.record_limit > 0 || params.operation_limit > 0)
        && (params.from_at_ms.is_none() || params.to_at_ms.is_none())
    {
        return Err(invalid_params(
            "finite fromAtMs and toAtMs are required for review or details",
        ));
    }
    if params.record_limit > 50 || params.operation_limit > 50 {
        return Err(invalid_params(
            "local usage page limits must be 0 through 50",
        ));
    }
    if params.from_at_ms.is_some() != params.to_at_ms.is_some() {
        return Err(invalid_params(
            "fromAtMs and toAtMs must be supplied together",
        ));
    }
    if params.from_at_ms.is_none()
        && [&params.account_id, &params.repository_id, &params.thread_id]
            .into_iter()
            .filter(|id| id.is_some())
            .count()
            > 1
    {
        return Err(invalid_params(
            "finite fromAtMs and toAtMs are required for combined identity filters",
        ));
    }
    if params.from_at_ms.is_some_and(|value| value < 0)
        || params.to_at_ms.is_some_and(|value| value < 0)
        || params
            .from_at_ms
            .zip(params.to_at_ms)
            .is_some_and(|(from, to)| to < from)
    {
        return Err(invalid_params("invalid local usage time range"));
    }
    if params.records_after.is_some() && params.record_limit == 0
        || params.operations_after.is_some() && params.operation_limit == 0
        || (params.records_after.is_some() || params.operations_after.is_some())
            && params.expected_snapshot.is_none()
    {
        return Err(invalid_params(
            "continuation requires a page limit and expectedSnapshot",
        ));
    }
    for id in [
        params.account_id.as_deref(),
        params.repository_id.as_deref(),
        params.thread_id.as_deref(),
        params.expected_snapshot.as_deref(),
        params
            .records_after
            .as_ref()
            .map(|cursor| cursor.id.as_str()),
        params
            .operations_after
            .as_ref()
            .map(|cursor| cursor.id.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            return Err(invalid_params("invalid local usage identity or cursor"));
        }
    }
    Ok(())
}

fn read(
    codex_home: PathBuf,
    params: LocalUsageSummaryParams,
) -> Result<LocalUsageSummaryResponse, JSONRPCErrorError> {
    let view = local_usage_snapshots::open(&codex_home, params.expected_snapshot.as_deref())
        .map_err(|error| match error {
            ViewError::Expired => {
                invalid_params("local usage snapshot changed; restart pagination")
            }
            ViewError::Unavailable => unavailable(()),
        })?;
    if let Some(mut response) = view.response(&params).map_err(unavailable)? {
        response.generated_at = now_ms()?;
        return Ok(response);
    }
    let binding_source = Bindings::merge_request(
        view.sidecar.as_ref(),
        params.work_context_bindings.as_deref(),
    )
    .map_err(|_| invalid_params("invalid or conflicting work context declarations"))?;
    let reader = &view.reader;
    let manifest = reader.manifest();
    let recorded = if let Some(source) = &binding_source {
        let recorded = reader
            .operation_binding_metadata(&source.operation_ids())
            .map_err(unavailable)?;
        source
            .validate_metadata(&recorded, params.repository_id.as_deref())
            .map_err(|_| {
                invalid_params("work context conflicts with recorded operation identity")
            })?;
        recorded
    } else {
        Vec::new()
    };
    let range = if params.from_at_ms.is_none() && params.to_at_ms.is_none() {
        TimeRange::all()
    } else {
        TimeRange::from_millis(
            params.from_at_ms.unwrap_or(0),
            params.to_at_ms.unwrap_or(i64::MAX),
        )
        .map_err(|_| invalid_params("invalid local usage time range"))?
    };
    let filter = UsageFilter {
        account_id: params.account_id.clone(),
        repository_id: params.repository_id.clone(),
        thread_id: params.thread_id.clone(),
        ..Default::default()
    };
    if range == TimeRange::all() && !reader.can_read_metadata_summary(&filter) {
        let mut error =
            internal_error("unbounded usage metadata is awaiting correction publication");
        error.data = Some(
            json!({"source":"usage-v2","coverage":"unavailable","reason":"metadata_corrections_pending","retryable":true}),
        );
        return Err(error);
    }
    let native_summary = reader.summary(range, &filter).map_err(unavailable)?;
    // Unbounded requests stay on the precomputed summary path. Rich facts are
    // an explicitly finite-window query, never a foreground full-history scan.
    let facts = if params.from_at_ms.is_some() && params.to_at_ms.is_some() {
        Some(
            reader
                .performance_review(range, &filter)
                .map_err(unavailable)?,
        )
    } else {
        None
    };
    let generated_at = now_ms()?;
    let snapshot = LocalUsageSnapshot {
        source_watermark: view.watermark.clone(),
        generated_at: view.captured_at_ms,
        generation: manifest.snapshot_generation,
        watermark: manifest.watermark,
        journal_watermark: reader.journal_watermark(),
        watermark_at_ms: manifest.watermark_at_ms,
        updated_at_ms: manifest.updated_at.saturating_mul(1_000),
        freshness: match reader.freshness() {
            Freshness::Fresh => "fresh",
            Freshness::Stale => "stale",
            Freshness::Unknown => "unknown",
        }
        .into(),
        pending_entries: reader.pending_entries(),
        pending_corrections: manifest.pending_corrections.len() as u64,
        refresh_id: None,
    };
    let mut report = facts.as_ref().map_or_else(
        || metadata_report(&params, &native_summary),
        |facts| report(&params, facts, &native_summary, &snapshot),
    );
    let mut review = if params.include_review {
        facts
            .as_ref()
            .map(|facts| review(facts, &report, &native_summary, &snapshot))
    } else {
        None
    };
    let (work_contexts, operation_bindings) = if let Some(source) = &binding_source {
        let evidence = if facts.is_some() {
            reader
                .bound_operation_effort(range, &filter, &source.operation_ids())
                .map_err(unavailable)?
        } else {
            Vec::new()
        };
        let selected = if facts.is_some() {
            evidence
                .iter()
                .map(|fact| fact.operation_id.clone())
                .collect()
        } else {
            recorded
                .iter()
                .filter(|fact| {
                    filter
                        .account_id
                        .as_ref()
                        .is_none_or(|id| fact.account_id.as_ref() == Some(id))
                        && filter
                            .repository_id
                            .as_ref()
                            .is_none_or(|id| fact.repository_id.as_ref() == Some(id))
                        && filter
                            .thread_id
                            .as_ref()
                            .is_none_or(|id| fact.thread_id.as_ref() == Some(id))
                })
                .map(|fact| fact.operation_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let bound = if facts.is_some() {
            evidence
                .iter()
                .try_fold(0u64, |sum, fact| sum.checked_add(fact.operations))
                .ok_or_else(|| unavailable(()))?
        } else {
            recorded
                .iter()
                .filter(|fact| {
                    selected.contains(&fact.operation_id)
                        && fact
                            .kind
                            .is_some_and(|kind| kind != ReviewOperationKind::Wait)
                })
                .count() as u64
        };
        let unknown = count(&native_summary["counts"]["operations"])
            .checked_sub(bound)
            .ok_or_else(|| unavailable(()))?;
        let coverage = if selected.is_empty() {
            "unattributed"
        } else if unknown == 0 {
            "recorded"
        } else {
            "partial"
        };
        report["workBindings"] = json!({"schemaVersion":1,"coverage":coverage,"basis":source.basis,"boundOperations":bound,"unknownOperations":unknown,"declaredOperations":source.operation_ids().len(),"matchedDeclarations":selected.len(),"unmatchedDeclarations":source.operation_ids().len().saturating_sub(selected.len()),"sourceRevision":source.revision});
        report["coverage"]["workBindings"] = json!(coverage);
        if let Some(review) = &mut review
            && let Some(facts) = &facts
        {
            review["outcomes"] = source
                .outcomes(
                    facts,
                    &review["outcomes"]["totals"],
                    &evidence,
                    params.workstream_id.as_deref(),
                )
                .map_err(unavailable)?;
            review["coverage"]["unavailable_reasons"]["work_binding_unobserved"] = json!(unknown);
            review["semantics"]["coverage"] = json!(
                "Work-context attribution uses only explicit request/compatibility operation IDs. Missing or retired evidence stays unattributed. Completed operations do not prove delivery."
            );
        }
        source.selected_ids(&selected)
    } else {
        (Vec::new(), Vec::new())
    };
    let (observations, next_records_cursor) = if params.record_limit == 0 {
        (None, None)
    } else {
        let after = params.records_after.clone().map(cursor);
        let page = reader
            .page(
                range,
                &filter,
                RecordKind::All,
                after.as_ref(),
                params.record_limit as usize,
            )
            .map_err(unavailable)?;
        (
            Some(
                page.records
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(unavailable)?,
            ),
            page.next.map(api_cursor),
        )
    };
    let (operations, next_operations_cursor) = if params.operation_limit == 0 {
        (None, None)
    } else {
        let after = params.operations_after.clone().map(cursor);
        let page = reader
            .operation_page(
                range,
                &filter,
                OperationSelection::All,
                after.as_ref(),
                params.operation_limit as usize,
            )
            .map_err(unavailable)?;
        (
            Some(
                page.data
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(unavailable)?,
            ),
            page.next_cursor.map(api_cursor),
        )
    };
    let response = LocalUsageSummaryResponse {
        contract_version: 1,
        source: "usage-v2".into(),
        generated_at,
        snapshot,
        report,
        summary: native_summary,
        review,
        observations,
        next_records_cursor,
        operations,
        next_operations_cursor,
        work_contexts,
        operation_bindings,
    };
    let encoded_bytes = serde_json::to_vec(&response).map_err(unavailable)?.len();
    if encoded_bytes > MAX_RESPONSE_BYTES {
        return Err(internal_error(
            "local usage response exceeds byte limit; request a narrower scope",
        ));
    }
    view.remember(&params, &response, encoded_bytes)
        .map_err(unavailable)?;
    Ok(response)
}

pub(super) fn now_ms() -> Result<u64, JSONRPCErrorError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| internal_error("local usage clock unavailable"))?
            .as_millis(),
    )
    .map_err(|_| internal_error("local usage clock unavailable"))
}

fn metadata_report(params: &LocalUsageSummaryParams, summary: &Value) -> Value {
    let (scope, id) = if let Some(id) = &params.repository_id {
        ("repository", Some(id))
    } else if let Some(id) = &params.thread_id {
        ("thread", Some(id))
    } else if let Some(id) = &params.account_id {
        ("account", Some(id))
    } else {
        ("all", None)
    };
    json!({"schemaVersion":1,"kind":"usageSummary","databaseSchemaVersion":codex_usage::MANIFEST_FORMAT_VERSION,"taxonomyVersion":codex_usage::USAGE_TAXONOMY_VERSION,
        "scope":{"type":scope,"id":id},"timeRange":Value::Null,"counts":summary["counts"],"providerTokens":[],"providerTokensByActivity":[],
        "coverage":{"state":if summary["records"] == 0 {"unobserved"} else {"partial"},"hasGaps":true,"providerTotals":"unavailable_unbounded","performance":"requires_finite_range","workBindings":"unattributed","details":summary["coverage"]},
        "measuredExecutionIntervalSumMs":Value::Null,"workBindings":{"coverage":"unattributed","basis":"unobserved","boundOperations":0,"unknownOperations":summary["counts"]["operations"]},
        "filters":{"accountId":params.account_id,"repositoryId":params.repository_id,"threadId":params.thread_id},"formulas":summary["formulas"]})
}

fn cursor(value: LocalUsageCursor) -> RecordCursor {
    RecordCursor::from_millis(value.observed_at_ms, value.id)
}

fn api_cursor(value: RecordCursor) -> LocalUsageCursor {
    LocalUsageCursor {
        observed_at_ms: value.observed_at_ms,
        id: value.id,
    }
}

fn unavailable<E>(_error: E) -> JSONRPCErrorError {
    // Storage errors may contain private paths. Never echo them over this API.
    internal_error("local usage reader unavailable")
}

fn rows(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn count(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}

fn report(
    params: &LocalUsageSummaryParams,
    facts: &Value,
    summary: &Value,
    snapshot: &LocalUsageSnapshot,
) -> Value {
    let (scope, id) = if let Some(id) = &params.repository_id {
        ("repository", Some(id))
    } else if let Some(id) = &params.thread_id {
        ("thread", Some(id))
    } else if let Some(id) = &params.account_id {
        ("account", Some(id))
    } else {
        ("all", None)
    };
    let state = if snapshot.pending_entries != 0
        || snapshot.pending_corrections != 0
        || snapshot.freshness != "fresh"
        || count(&facts["coverage"]["unknownTokenObservations"]) != 0
        || count(&facts["coverage"]["incompleteTokenObservations"]) != 0
        || count(&facts["coverage"]["modelRequestsWithoutProviderTotal"]) != 0
    {
        "partial"
    } else {
        match facts["coverage"]["collectionCompleteness"].as_str() {
            Some("recorded") => "complete",
            Some("unobserved") => "unobserved",
            _ => "partial",
        }
    };
    let provider_tokens = rows(&facts["tokens"]).iter().map(|token| {
        let category = match token["category"].as_str().unwrap_or("unknown") {
            "cached_input_tokens" => "input_tokens_details.cached_tokens",
            "reasoning_output_tokens" => "output_tokens_details.reasoning_tokens",
            value => value,
        };
        json!({"category":category,"repositoryBucket":params.repository_id.as_deref().unwrap_or("all"),
            "measurementProvenance":"provider_reported","measuredTokens":token["measuredTokens"],
            "exactTokens":token["exactTokens"],"unknownObservations":token["unknownObservations"],"observationCount":token["observations"]})
    }).collect::<Vec<_>>();
    let activities = rows(&facts["tokensByActivity"]).iter().map(|row| json!({
        "phase":row["phase"],"activity":row["activity"],"attributionProvenance":row["provenance"],
        "measuredTokens":row["measuredTokens"],"exactTokens":Value::Null,"unknownObservations":row["unknownObservations"]
    })).collect::<Vec<_>>();
    json!({"schemaVersion":1,"kind":"usageSummary","databaseSchemaVersion":codex_usage::MANIFEST_FORMAT_VERSION,
        "taxonomyVersion":codex_usage::USAGE_TAXONOMY_VERSION,"scope":{"type":scope,"id":id},
        "timeRange":if params.from_at_ms.is_some() || params.to_at_ms.is_some() {json!({"startMs":params.from_at_ms.unwrap_or(0),"endMs":params.to_at_ms.unwrap_or(i64::MAX)})} else {Value::Null},
        "coverage":{"state":state,"hasGaps":state != "complete","workBindings":"unattributed","details":facts["coverage"]},
        "counts":summary["counts"],"providerTokens":provider_tokens,"providerTokensByActivity":activities,
        "measuredExecutionIntervalSumMs":facts["counts"]["interval_sum_ms"],
        "workBindings":{"coverage":"unattributed","basis":"unobserved","boundOperations":0,"unknownOperations":summary["counts"]["operations"]},
        "filters":{"accountId":params.account_id,"repositoryId":params.repository_id,"threadId":params.thread_id}})
}

/// A consumer-neutral review projection. Unknown measurements remain nullable;
/// only observed operation counts and measured interval sums are populated.
fn review(facts: &Value, report: &Value, summary: &Value, snapshot: &LocalUsageSnapshot) -> Value {
    let operations = count(&facts["counts"]["operations"]);
    let token = |category: &str| {
        rows(&facts["tokens"])
            .iter()
            .find(|row| row["category"] == category)
    };
    let exact =
        |category: &str| token(category).map_or(Value::Null, |row| row["exactTokens"].clone());
    let mut events = BTreeMap::<String, u64>::new();
    for gap in rows(&facts["coverage"]["coverageEvents"]) {
        if let Some(state) = gap["state"].as_str() {
            let value = events.entry(state.into()).or_default();
            *value = value.saturating_add(count(&gap["count"]));
        }
    }
    let mut effort = facts["outcomes"]["totals"].clone();
    // These are counts of observed links, not estimates for missing links.
    effort["retryOperations"] = json!(count(&effort["retryOperations"]));
    // The reader's interval sums are diagnostic effort, not a wall union.
    effort["elapsedExecutionMs"] = json!({"measured":0,"exact":Value::Null,"unknown":operations});
    effort["recordedWaitMs"]["unknown"] = json!(0);
    let zero_measurement = json!({"measured":0,"exact":Value::Null,"unknown":0});
    let attributed = json!({"operations":0,"retryOperations":0,"reworkOperations":0,
        "providerTotalTokens":zero_measurement,"activeAgentMs":zero_measurement,
        "elapsedExecutionMs":zero_measurement,"recordedWaitMs":zero_measurement});
    let mut activity_totals = BTreeMap::<(String, String), (u64, BTreeMap<String, u64>)>::new();
    for row in rows(&facts["tokensByActivity"]) {
        let key = (
            row["phase"].as_str().unwrap_or("unknown").to_owned(),
            row["activity"].as_str().unwrap_or("unknown").to_owned(),
        );
        let (total, provenance) = activity_totals.entry(key).or_default();
        let measured = count(&row["measuredTokens"]);
        *total = total.saturating_add(measured);
        let value = provenance
            .entry(row["provenance"].as_str().unwrap_or("unknown").to_owned())
            .or_default();
        *value = value.saturating_add(measured);
    }
    let activities = activity_totals
        .into_iter()
        .map(|((phase, activity), (total, provenance))| {
            json!({"phase":phase,"activity":activity,"total_tokens":total,
            "share":Value::Null,"operations":Value::Null,"provenance":provenance})
        })
        .collect::<Vec<_>>();
    let mut families = BTreeMap::<String, u64>::new();
    for row in rows(&facts["tools"]) {
        if let Some(tool) = row["tool"].as_str() {
            let total = families.entry(tool.into()).or_default();
            *total = total.saturating_add(count(&row["count"]));
        }
    }
    let families = families
        .into_iter()
        .map(|(family, count)| json!({"family":family,"count":count}))
        .collect::<Vec<_>>();
    json!({
        "coverage":{"state":report["coverage"]["state"],"has_gaps":report["coverage"]["hasGaps"],
            "configured_collectors":1,"available_collectors":1,"contributing_collectors":u32::from(operations > 0 || count(&facts["coverage"]["rawTokenObservations"]) > 0),
            "freshest_at_ms":(snapshot.watermark_at_ms > 0).then_some(snapshot.watermark_at_ms),
            "events":events,"token_observations":{"observed":facts["coverage"]["rawTokenObservations"],"unknown":facts["coverage"]["unknownTokenObservations"]},
            "unavailable_reasons":{"work_binding_unobserved":operations},"database_schemas":[codex_usage::MANIFEST_FORMAT_VERSION],"taxonomy_versions":[codex_usage::USAGE_TAXONOMY_VERSION]},
        "totals":{"total_tokens":exact("total_tokens"),"input_tokens":exact("input_tokens"),"cached_input_tokens":exact("cached_input_tokens"),
            "output_tokens":exact("output_tokens"),"reasoning_tokens":exact("reasoning_output_tokens"),
            "model_requests":facts["counts"]["model_requests"],"tool_calls":facts["counts"]["tools"],"operations":operations},
        "activities":activities,
        "time":{"request_to_delivery":{"measured_ms":0,"unknown_intervals":operations},
            "execution_wall":{"measured_ms":0,"unknown_intervals":operations},
            "summed_agent_active":{"measured_ms":0,"unknown_intervals":operations},"phases":[]},
        "tools":{"outcomes":summary["tools"]["outcomes"],"families":families},
        "semantics":{"tokens":"Provider totals are reported, not inferred. Cached input and reasoning output are subsets; do not add them again.",
            "time":"Wall-clock, delivery, and active-agent intervals are unobserved. Only report.measuredExecutionIntervalSumMs retains operation interval sums; overlap is not measured elapsed wall time.",
            "coverage":"Missing work bindings remain unattributed. No task, workstream, or delivery outcome is inferred. Retry/wait counts include observed links only; unobserved families are listed in report.coverage.details."},
        "outcomes":{"schemaVersion":1,"coverage":"unattributed","totals":effort,"attributed":attributed,"unattributed":effort,
            "unattributedReasons":{"work_binding_unobserved":operations},"rows":[],"totalRows":0,"nextCursor":Value::Null,
            "basis":"No explicit work-context bindings are captured by this producer; operation completion does not prove delivery."}
    })
}
