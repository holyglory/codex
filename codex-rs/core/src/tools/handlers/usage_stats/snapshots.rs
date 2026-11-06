//! Bounded continuation leases. No warming worker or report refresh is needed.
use super::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

const PREFIX: &str = "usage-v1:";
const MAX_SNAPSHOTS: usize = 8;
const LIFETIME: Duration = Duration::from_secs(10 * 60);

struct Pinned {
    id: String,
    created: Instant,
    context: (PathBuf, String, PathBuf),
    query: Value,
    reader: Arc<UsageReader>,
}

fn retained() -> &'static Mutex<VecDeque<Pinned>> {
    static SNAPSHOTS: OnceLock<Mutex<VecDeque<Pinned>>> = OnceLock::new();
    SNAPSHOTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub(super) fn execute_request(
    context: &UsageStatsContext,
    mut args: UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    let mut signature = serde_json::to_value(&args).map_err(|_| storage_error())?;
    for field in [
        "cursor_id",
        "cursor_sort_value",
        "outcome_cursor",
        "limit",
        "outcome_limit",
    ] {
        signature
            .as_object_mut()
            .expect("argument struct")
            .remove(field);
    }
    let owner = (
        context.codex_home.clone(),
        context.thread_id.clone(),
        context.cwd.clone(),
    );
    let continuation = args
        .cursor_id
        .as_deref()
        .filter(|id| id.starts_with(PREFIX))
        .map(|id| {
            let (snapshot, row) = id[PREFIX.len()..]
                .split_once(':')
                .ok_or_else(|| tool_error("usage cursor is invalid"))?;
            Ok::<_, FunctionCallError>((snapshot.to_string(), row.to_string()))
        })
        .transpose()?;
    let (reader, snapshot_id) = if let Some((id, row)) = continuation {
        let mut retained = retained()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retained.retain(|entry| entry.created.elapsed() < LIFETIME);
        let entry = retained
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| tool_error("usage cursor expired; request a new first page"))?;
        if entry.context != owner || entry.query != signature {
            return Err(tool_error("usage cursor does not match this query"));
        }
        args.cursor_id = Some(row);
        (Arc::clone(&entry.reader), Some(id))
    } else {
        (
            Arc::new(UsageReader::open(&context.codex_home).map_err(|_| storage_error())?),
            None,
        )
    };
    let mut value = execute(&reader, context, args)?;
    if let Some(cursor) = value.get_mut("nextCursor").and_then(Value::as_object_mut)
        && let Some(row) = cursor.get("id").and_then(Value::as_str).map(str::to_string)
    {
        let id = snapshot_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let mut retained = retained()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retained.retain(|entry| entry.created.elapsed() < LIFETIME);
        if !retained.iter().any(|entry| entry.id == id) {
            while retained.len() >= MAX_SNAPSHOTS {
                retained.pop_front();
            }
            retained.push_back(Pinned {
                id: id.clone(),
                created: Instant::now(),
                context: owner,
                query: signature,
                reader,
            });
        }
        let remaining = retained
            .iter()
            .find(|entry| entry.id == id)
            .map_or(Duration::ZERO, |entry| {
                LIFETIME.saturating_sub(entry.created.elapsed())
            });
        cursor.insert("id".to_string(), json!(format!("{PREFIX}{id}:{row}")));
        cursor.insert("expiresAfterMs".to_string(), json!(remaining.as_millis()));
    }
    Ok(value)
}
