//! Optional, explicitly declared operation bindings. This is a bounded read-only
//! sidecar, not a second observation database or an attribution heuristic.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_app_server_protocol::LocalUsageOperationBinding;
use codex_app_server_protocol::LocalUsageWorkContext;
use codex_usage::BoundOperationEffort;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const MAX_BYTES: u64 = 512 * 1024;
const MAX_BINDINGS: usize = 256;
const MAX_CONTEXTS: usize = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Line {
    #[serde(alias = "schema_version")]
    schema_version: u32,
    #[serde(alias = "operation_id")]
    operation_id: String,
    #[serde(alias = "captured_at_ms")]
    captured_at_ms: i64,
    #[serde(alias = "work_context")]
    work_context: Context,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Context {
    #[serde(alias = "schema_version")]
    schema_version: u32,
    id: String,
    #[serde(alias = "repository_id")]
    repository_id: Option<String>,
    #[serde(alias = "project_id")]
    project_id: Option<String>,
    #[serde(alias = "workstream_id")]
    workstream_id: Option<String>,
    #[serde(alias = "outcome_id")]
    outcome_id: Option<String>,
}

pub(super) struct Bindings {
    pub(super) revision: String,
    contexts: BTreeMap<String, LocalUsageWorkContext>,
    operations: BTreeMap<String, LocalUsageOperationBinding>,
}

pub(super) fn available(home: &Path) -> bool {
    Bindings::read(home).is_ok_and(|source| source.is_some())
}

impl Bindings {
    pub(super) fn read(home: &Path) -> Result<Option<Self>, ()> {
        let path = home
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("work-contexts.jsonl");
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(()),
        };
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(());
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .map_err(|_| ())?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(());
        }
        let mut source = Self {
            revision: String::new(),
            contexts: BTreeMap::new(),
            operations: BTreeMap::new(),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
        let mut lines = 0;
        let mut latest = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            lines += 1;
            if lines > MAX_BINDINGS {
                return Err(());
            }
            let line: Line = serde_json::from_str(line).map_err(|_| ())?;
            if line.schema_version != 1
                || line.work_context.schema_version != 1
                || line.captured_at_ms < 0
            {
                return Err(());
            }
            latest = latest.max(line.captured_at_ms);
            let context = LocalUsageWorkContext {
                schema_version: line.work_context.schema_version,
                id: line.work_context.id,
                repository_id: line.work_context.repository_id,
                project_id: line.work_context.project_id,
                workstream_id: line.work_context.workstream_id,
                outcome_id: line.work_context.outcome_id,
            };
            for id in [
                Some(line.operation_id.as_str()),
                Some(context.id.as_str()),
                context.repository_id.as_deref(),
                context.project_id.as_deref(),
                context.workstream_id.as_deref(),
                context.outcome_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if id.is_empty()
                    || id.len() > 256
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
                {
                    return Err(());
                }
            }
            if source
                .contexts
                .get(&context.id)
                .is_some_and(|prior| prior != &context)
            {
                return Err(());
            }
            let binding = LocalUsageOperationBinding {
                schema_version: 1,
                operation_id: line.operation_id,
                work_context_id: context.id.clone(),
                captured_at_ms: line.captured_at_ms,
            };
            if source
                .operations
                .get(&binding.operation_id)
                .is_some_and(|prior| prior != &binding)
            {
                return Err(());
            }
            source.contexts.insert(context.id.clone(), context);
            source
                .operations
                .insert(binding.operation_id.clone(), binding);
            if source.contexts.len() > MAX_CONTEXTS {
                return Err(());
            }
        }
        source.revision = revision(path, Sha256::digest(&bytes).into(), lines, latest)?;
        Ok(Some(source))
    }

    pub(super) fn operation_ids(&self) -> BTreeSet<String> {
        self.operations.keys().cloned().collect()
    }

    /// Return only declarations whose exact operation has scoped evidence.
    pub(super) fn selected(
        &self,
        evidence: &[BoundOperationEffort],
    ) -> (Vec<LocalUsageWorkContext>, Vec<LocalUsageOperationBinding>) {
        let bindings = evidence
            .iter()
            .filter_map(|row| self.operations.get(&row.operation_id).cloned())
            .collect::<Vec<_>>();
        let contexts = bindings
            .iter()
            .filter_map(|binding| self.contexts.get(&binding.work_context_id))
            .map(|context| (context.id.clone(), context.clone()))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        (contexts, bindings)
    }

    pub(super) fn outcomes(
        &self,
        facts: &Value,
        total: &Value,
        evidence: &[BoundOperationEffort],
    ) -> Result<Value, ()> {
        let mut attributed = zero_effort();
        let mut attributed_observations = 0u64;
        let mut groups = BTreeMap::<String, (Option<String>, Value)>::new();
        for fact in evidence {
            let binding = self.operations.get(&fact.operation_id).ok_or(())?;
            let context = self.contexts.get(&binding.work_context_id).ok_or(())?;
            let Some(outcome) = &context.outcome_id else {
                continue;
            };
            attributed_observations = attributed_observations
                .checked_add(fact.token_observations)
                .ok_or(())?;
            let mut effort = zero_effort();
            effort["operations"] = json!(fact.operations);
            effort["retryOperations"] = json!(fact.retry_operations);
            effort["reworkOperations"] = json!(fact.rework_operations);
            effort["providerTotalTokens"] = json!({"measured":fact.provider_total_tokens,"exact":Value::Null,"unknown":fact.unknown_token_observations});
            effort["activeAgentMs"]["unknown"] = json!(fact.operations);
            effort["elapsedExecutionMs"] = json!({"measured":fact.execution_interval_sum_ms,"exact":Value::Null,"unknown":fact.unknown_execution_intervals});
            effort["recordedWaitMs"]["measured"] = json!(fact.recorded_wait_ms);
            combine(&mut attributed, &effort, false)?;
            let (workstream, aggregate) = groups
                .entry(outcome.clone())
                .or_insert_with(|| (context.workstream_id.clone(), zero_effort()));
            if workstream != &context.workstream_id {
                return Err(());
            }
            combine(aggregate, &effort, false)?;
        }
        let mut unattributed = total.clone();
        combine(&mut unattributed, &attributed, true)?;
        let unattributed_observations = facts["coverage"]["rawTokenObservations"]
            .as_u64()
            .ok_or(())?
            .checked_sub(attributed_observations)
            .ok_or(())?;
        let rows = groups.into_iter().map(|(outcome_id,(workstream_id,effort))| json!({"outcomeId":outcome_id,"workstreamId":workstream_id,"title":Value::Null,"effort":effort})).collect::<Vec<_>>();
        let full = unattributed["operations"] == 0
            && unattributed_observations == 0
            && unattributed["providerTotalTokens"]["measured"] == 0
            && unattributed["providerTotalTokens"]["unknown"] == 0
            && unattributed["elapsedExecutionMs"]["measured"] == 0
            && unattributed["elapsedExecutionMs"]["unknown"] == 0
            && unattributed["recordedWaitMs"]["measured"] == 0
            && facts["coverage"]["collectionCompleteness"] == "recorded";
        Ok(
            json!({"schemaVersion":1,"coverage":if rows.is_empty() {"unattributed"} else if full {"complete"} else {"partial"},
            "totals":total,"attributed":attributed,"unattributed":unattributed,
            "unattributedReasons":{"operation_context_or_outcome_unobserved":unattributed["operations"],"token_observation_context_unobserved":unattributed_observations},
            "totalRows":rows.len(),"rows":rows,"nextCursor":Value::Null,
            "basis":"Explicit version-1 sidecar declarations joined only by captured operation_id; missing or retired operation detail and response-only token links stay unattributed. Operation completion does not prove delivery."}),
        )
    }
}

// Content digests stay private. A bounded process-local registry gives changed
// bytes a new readable revision, even if line count/timestamps were reused.
fn revision(path: PathBuf, digest: [u8; 32], lines: usize, latest: i64) -> Result<String, ()> {
    static SOURCES: OnceLock<Mutex<BTreeMap<PathBuf, ([u8; 32], String)>>> = OnceLock::new();
    static SERIAL: AtomicU64 = AtomicU64::new(1);
    static SESSION: OnceLock<u128> = OnceLock::new();
    let mut sources = SOURCES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| ())?;
    if let Some((prior, revision)) = sources.get(&path)
        && prior == &digest
    {
        return Ok(revision.clone());
    }
    let session = SESSION.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |value| value.as_nanos())
    });
    let revision = format!(
        "b{lines}-{latest}-r{}-{session}",
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    if sources.len() >= 32 {
        sources.clear();
    }
    sources.insert(path, (digest, revision.clone()));
    Ok(revision)
}

pub(super) fn zero_effort() -> Value {
    let measurement = json!({"measured":0,"exact":Value::Null,"unknown":0});
    json!({"operations":0,"retryOperations":0,"reworkOperations":0,"providerTotalTokens":measurement,"activeAgentMs":measurement,"elapsedExecutionMs":measurement,"recordedWaitMs":measurement})
}

fn combine(target: &mut Value, delta: &Value, subtract: bool) -> Result<(), ()> {
    let amount = |a: u64, b: u64| {
        if subtract {
            a.checked_sub(b)
        } else {
            a.checked_add(b)
        }
        .ok_or(())
    };
    for key in ["operations", "retryOperations", "reworkOperations"] {
        target[key] = json!(amount(
            target[key].as_u64().ok_or(())?,
            delta[key].as_u64().ok_or(())?
        )?);
    }
    for key in [
        "providerTotalTokens",
        "activeAgentMs",
        "elapsedExecutionMs",
        "recordedWaitMs",
    ] {
        for component in ["measured", "unknown"] {
            target[key][component] = json!(amount(
                target[key][component].as_u64().ok_or(())?,
                delta[key][component].as_u64().ok_or(())?
            )?);
        }
        // Partial measurements are lower bounds, never exact per-outcome totals.
        target[key]["exact"] = Value::Null;
    }
    Ok(())
}
