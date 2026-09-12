use super::DEFAULT_PAGE_LIMIT;
use super::MAX_OUTPUT_BYTES;
use super::MAX_PAGE_LIMIT;
use super::UsageStatsArgs;
use super::tool_error;
use crate::function_tool::FunctionCallError;
use serde_json::Value;
use serde_json::json;
use sha1::Digest;
use sha1::Sha1;

/// Page detailed dimensions while repeating the report's aggregate totals.
pub(super) fn paginate(value: &mut Value, args: &UsageStatsArgs) -> Result<(), FunctionCallError> {
    let limit = args.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if limit == 0 || limit > MAX_PAGE_LIMIT {
        return Err(tool_error("usage summary limit must be between 1 and 50"));
    }
    if args.cursor_id.is_some() != args.cursor_sort_value.is_some() {
        return Err(tool_error("summary cursor fields must be supplied together"));
    }
    let encoded = serde_json::to_vec(value).map_err(|_| tool_error("cannot encode usage summary"))?;
    let budget = MAX_OUTPUT_BYTES - 512;
    if args.limit.is_none() && args.cursor_id.is_none() && encoded.len() <= budget {
        return Ok(());
    }
    // This identifies the query, not an authorization grant or an immutable database snapshot.
    let identity = json!([value["kind"], value["scope"], value["rootThreadId"],
                          value["includeDescendants"], value["account"], value["timeRange"]]);
    let cursor_id = format!("summary-v1-{:x}", Sha1::digest(identity.to_string().as_bytes()));
    if args.cursor_id.as_ref().is_some_and(|id| id != &cursor_id) {
        return Err(tool_error("summary cursor belongs to a different query"));
    }
    let offset = usize::try_from(args.cursor_sort_value.unwrap_or(0))
        .map_err(|_| tool_error("invalid summary cursor offset"))?;
    let fields: &[&str] = if value["kind"] == "taskTreeSummary" {
        &["agents"]
    } else {
        &["providerTokensByActivity", "classifications"]
    };
    let mut rows = Vec::new();
    for &field in fields {
        let values = value[field].as_array_mut().ok_or_else(|| tool_error("invalid summary details"))?;
        rows.extend(std::mem::take(values).into_iter().map(|row| (field, row)));
    }
    if offset > rows.len() {
        return Err(tool_error("summary changed; restart pagination"));
    }
    let base = value.clone();
    let mut end = offset.saturating_add(limit as usize).min(rows.len());
    loop {
        *value = base.clone();
        for (field, row) in &rows[offset..end] {
            value[*field].as_array_mut().expect("validated detail array").push(row.clone());
        }
        value["pagination"] = json!({
            "sections": fields,
            "offset": offset,
            "returnedRows": end - offset,
            "totalRows": rows.len(),
            "nextCursor": (end < rows.len()).then(|| json!({"sortValue": end, "id": cursor_id})),
            "totalsRepeated": true,
            "consistency": "current observation; repeat the same scope and time range on every page"
        });
        if serde_json::to_vec(value).map_err(|_| tool_error("cannot encode usage summary"))?.len() <= budget {
            return Ok(());
        }
        if end == offset {
            return Err(tool_error("summary totals exceed the safe bound; narrow the time range"));
        }
        end -= 1;
        if end == offset && offset < rows.len() {
            return Err(tool_error("one summary detail exceeds the safe bound"));
        }
    }
}
