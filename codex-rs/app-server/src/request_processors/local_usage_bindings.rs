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
use codex_app_server_protocol::LocalUsageWorkContextBinding;
use codex_usage::BoundOperationEffort;
use codex_usage::OperationBindingMetadata;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const MAX_BYTES: u64 = 512 * 1024;
pub(super) const MAX_BINDINGS: usize = 256;

#[derive(Clone, Default)]
pub(super) struct Bindings {
    pub(super) revision: String,
    pub(super) basis: &'static str,
    contexts: BTreeMap<String, LocalUsageWorkContext>,
    operations: BTreeMap<String, LocalUsageOperationBinding>,
}

impl Bindings {
    pub(super) fn outcomes_with_sparse(
        source: Option<&Self>,
        facts: &Value,
        total: &Value,
        evidence: &[BoundOperationEffort],
        workstream: Option<&str>,
        sparse: &[codex_usage::BoundWorkEffort],
    ) -> Result<Value, ()> {
        let mut rows =
            source.map_or_else(|| Ok(Vec::new()), |source| source.bound_rows(evidence))?;
        rows.extend(
            sparse
                .iter()
                .map(|row| (sparse_context(&row.context), row.effort.clone())),
        );
        outcomes_from_contexts(facts, total, &rows, workstream)
    }

    fn bound_rows(
        &self,
        evidence: &[BoundOperationEffort],
    ) -> Result<Vec<(LocalUsageWorkContext, codex_usage::WorkEffort)>, ()> {
        evidence
            .iter()
            .map(|fact| {
                let binding = self.operations.get(&fact.operation_id).ok_or(())?;
                let context = self.contexts.get(&binding.work_context_id).ok_or(())?;
                Ok((context.clone(), codex_usage::WorkEffort::from(fact)))
            })
            .collect()
    }

    pub(super) fn merge_request(
        sidecar: Option<&Self>,
        declarations: Option<&[LocalUsageWorkContextBinding]>,
    ) -> Result<Option<Self>, ()> {
        if declarations.is_some_and(|rows| rows.len() > MAX_BINDINGS) {
            return Err(());
        }
        if sidecar.is_none() && declarations.is_none() {
            return Ok(None);
        }
        let mut source = sidecar.cloned().unwrap_or_default();
        for declaration in declarations.unwrap_or_default() {
            source.insert(declaration.clone())?;
        }
        if declarations.is_some() {
            source.basis = if sidecar.is_some() {
                "request_and_compatibility_declarations"
            } else {
                "request_declarations"
            };
            source.revision = format!("request-{}", source.operations.len());
        }
        Ok(Some(source))
    }

    fn insert(&mut self, line: LocalUsageWorkContextBinding) -> Result<(), ()> {
        if line.schema_version != 1
            || line.work_context.schema_version != 1
            || line.captured_at_ms < 0
        {
            return Err(());
        }
        let context = line.work_context;
        for id in [
            Some(line.operation_id.as_str()),
            Some(context.id.as_str()),
            context.repository_id.as_deref(),
            context.native_project_id.as_deref(),
            context.workstream_id.as_deref(),
            context.outcome_id.as_deref(),
            context.thread_id.as_deref(),
            context.turn_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !safe_identifier(id, /*experiment*/ false) {
                return Err(());
            }
        }
        if context
            .experiment_ref
            .as_deref()
            .is_some_and(|value| !safe_identifier(value, /*experiment*/ true))
        {
            return Err(());
        }
        if self
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
        if self
            .operations
            .get(&binding.operation_id)
            .is_some_and(|prior| prior != &binding)
        {
            return Err(());
        }
        self.contexts.insert(context.id.clone(), context);
        self.operations
            .insert(binding.operation_id.clone(), binding);
        if self.operations.len() > MAX_BINDINGS || self.contexts.len() > MAX_BINDINGS {
            return Err(());
        }
        Ok(())
    }

    pub(super) fn validate_metadata(
        &self,
        recorded: &[OperationBindingMetadata],
        repository: Option<&str>,
    ) -> Result<(), ()> {
        for fact in recorded {
            let binding = self.operations.get(&fact.operation_id).ok_or(())?;
            let context = self.contexts.get(&binding.work_context_id).ok_or(())?;
            if repository
                .zip(fact.repository_id.as_deref())
                .is_some_and(|(wanted, actual)| wanted != actual)
                || context
                    .thread_id
                    .as_ref()
                    .zip(fact.thread_id.as_ref())
                    .is_some_and(|(wanted, actual)| wanted != actual)
                || context
                    .turn_id
                    .as_ref()
                    .zip(fact.turn_id.as_ref())
                    .is_some_and(|(wanted, actual)| wanted != actual)
            {
                return Err(());
            }
        }
        Ok(())
    }

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
            basis: "explicit_sidecar",
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
            let line: LocalUsageWorkContextBinding = serde_json::from_str(line).map_err(|_| ())?;
            latest = latest.max(line.captured_at_ms);
            source.insert(line)?;
        }
        source.revision = revision(path, Sha256::digest(&bytes).into(), lines, latest)?;
        Ok(Some(source))
    }

    pub(super) fn operation_ids(&self) -> BTreeSet<String> {
        self.operations.keys().cloned().collect()
    }

    /// Return only declarations whose exact operation has scoped evidence.
    pub(super) fn selected_ids(
        &self,
        ids: &BTreeSet<String>,
    ) -> (Vec<LocalUsageWorkContext>, Vec<LocalUsageOperationBinding>) {
        let bindings = ids
            .iter()
            .filter_map(|id| self.operations.get(id).cloned())
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
        workstream: Option<&str>,
    ) -> Result<Value, ()> {
        outcomes_from_contexts(facts, total, &self.bound_rows(evidence)?, workstream)
    }
}

fn outcomes_from_contexts(
    facts: &Value,
    total: &Value,
    rows: &[(LocalUsageWorkContext, codex_usage::WorkEffort)],
    workstream: Option<&str>,
) -> Result<Value, ()> {
    let mut attributed = zero_effort();
    let mut attributed_observations = 0u64;
    let mut excluded = zero_effort();
    let mut excluded_observations = 0u64;
    let mut groups = BTreeMap::<(Option<String>, String, Option<String>), Value>::new();
    for (context, fact) in rows {
        let mut effort = zero_effort();
        effort["operations"] = json!(fact.operations);
        effort["retryOperations"] = json!(fact.retry_operations);
        effort["reworkOperations"] = json!(fact.rework_operations);
        effort["providerTotalTokens"] = json!({"measured":fact.provider_total_tokens,"exact":Value::Null,"unknown":fact.unknown_token_observations});
        effort["activeAgentMs"]["unknown"] = json!(fact.operations);
        effort["elapsedExecutionMs"]["unknown"] = json!(fact.operations);
        effort["recordedWaitMs"]["measured"] = json!(fact.recorded_wait_ms);
        if workstream
            .zip(context.workstream_id.as_deref())
            .is_some_and(|(selected, actual)| selected != actual)
        {
            combine(&mut excluded, &effort, /*subtract*/ false)?;
            excluded_observations = excluded_observations
                .checked_add(fact.token_observations)
                .ok_or(())?;
            continue;
        }
        let Some(outcome) = &context.outcome_id else {
            continue;
        };
        if workstream.is_some() && context.workstream_id.is_none() {
            continue;
        }
        attributed_observations = attributed_observations
            .checked_add(fact.token_observations)
            .ok_or(())?;
        combine(&mut attributed, &effort, /*subtract*/ false)?;
        let aggregate = groups
            .entry((
                context.native_project_id.clone(),
                outcome.clone(),
                context.workstream_id.clone(),
            ))
            .or_insert_with(zero_effort);
        combine(aggregate, &effort, /*subtract*/ false)?;
    }
    let mut scoped_total = total.clone();
    combine(&mut scoped_total, &excluded, /*subtract*/ true)?;
    let mut unattributed = scoped_total.clone();
    combine(&mut unattributed, &attributed, /*subtract*/ true)?;
    let unattributed_observations = facts["coverage"]["rawTokenObservations"]
        .as_u64()
        .ok_or(())?
        .checked_sub(excluded_observations)
        .ok_or(())?
        .checked_sub(attributed_observations)
        .ok_or(())?;
    let rows = groups.into_iter().map(|((native_project_id,outcome_id,workstream_id),effort)| json!({"outcomeId":outcome_id,"nativeProjectId":native_project_id,"workstreamId":workstream_id,"title":Value::Null,"effort":effort})).collect::<Vec<_>>();
    let full = unattributed["operations"] == 0
        && scoped_total["providerTotalTokens"]["unknown"] == 0
        && scoped_total["activeAgentMs"]["unknown"] == 0
        && scoped_total["elapsedExecutionMs"]["unknown"] == 0
        && scoped_total["recordedWaitMs"]["unknown"] == 0
        && unattributed_observations == 0
        && unattributed["providerTotalTokens"]["measured"] == 0
        && unattributed["providerTotalTokens"]["unknown"] == 0
        && unattributed["elapsedExecutionMs"]["measured"] == 0
        && unattributed["elapsedExecutionMs"]["unknown"] == 0
        && unattributed["recordedWaitMs"]["measured"] == 0
        && facts["coverage"]["collectionCompleteness"] == "recorded";
    Ok(
        json!({"schemaVersion":1,"coverage":if rows.is_empty() {"unattributed"} else if full {"complete"} else {"partial"},
            "totals":scoped_total,"attributed":attributed,"unattributed":unattributed,
            "unattributedReasons":{"operation_context_or_outcome_unobserved":unattributed["operations"],"token_observation_context_unobserved":unattributed_observations},
            "totalRows":rows.len(),"rows":rows,"nextCursor":Value::Null,
            "basis":"Explicit sparse assignment intervals and exceptional operation declarations joined to captured operation identity. Repository aggregates retain scope; outcome totals/rows honor the selected workstream and include unknown-stream work as unattributed. Elapsed execution union is unknown; interval sums are diagnostic only. Operation completion does not prove delivery."}),
    )
}

pub(super) fn sparse_context(context: &codex_usage::BoundWorkContext) -> LocalUsageWorkContext {
    LocalUsageWorkContext {
        schema_version: 1,
        id: context.context_id(),
        repository_id: None,
        native_project_id: Some(context.native_project_id.clone()),
        workstream_id: context.workstream_id.clone(),
        outcome_id: context.outcome_id.clone(),
        thread_id: context.thread_id.clone(),
        turn_id: None,
        experiment_ref: None,
    }
}

pub(super) fn safe_identifier(id: &str, experiment: bool) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte) || experiment && byte == b'@'
        })
}

// Content digests stay private. A bounded process-local registry gives changed
// bytes a new readable revision, even if line count/timestamps were reused.
fn revision(path: PathBuf, digest: [u8; 32], lines: usize, latest: i64) -> Result<String, ()> {
    type RevisionCache = BTreeMap<PathBuf, ([u8; 32], String)>;
    static SOURCES: OnceLock<Mutex<RevisionCache>> = OnceLock::new();
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
