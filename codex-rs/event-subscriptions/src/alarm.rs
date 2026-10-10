//! Generic one-shot reminders. Policy and business completion belong to callers.
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlarmState {
    Armed,
    Due,
    Delivered,
    Acknowledged,
    Cancelled,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    Completed,
    Failed,
    Denied,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationResult {
    pub operation_id: String,
    pub tool_name: String,
    pub outcome_class: OperationOutcome,
    pub result_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmSpec {
    pub dedupe_key: String,
    pub project_id: Option<String>,
    pub workstream_id: Option<String>,
    pub subject: String,
    pub summary: String,
    pub absolute_at_ms: Option<i64>,
    pub relative_ms: Option<i64>,
    pub active_work_ms: Option<i64>,
    pub operation_result: Option<OperationResult>,
    pub expires_at_ms: Option<i64>,
}

impl AlarmSpec {
    pub fn validate(&self, now_ms: i64) -> Result<(), &'static str> {
        for value in [&self.dedupe_key, &self.subject] {
            if value.trim().is_empty() || value.len() > 160 || value.chars().any(char::is_control) {
                return Err("alarm key and subject must be nonempty bounded text");
            }
        }
        if self.summary.len() > 1024 || self.summary.contains('\0') {
            return Err("alarm summary exceeds 1024 bytes or contains NUL");
        }
        for value in self.project_id.iter().chain(self.workstream_id.iter()) {
            if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
                return Err("invalid alarm scope");
            }
        }
        if [
            self.absolute_at_ms.is_some(),
            self.relative_ms.is_some(),
            self.active_work_ms.is_some(),
            self.operation_result.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
            != 1
        {
            return Err("provide exactly one alarm trigger");
        }
        if self.absolute_at_ms.is_some_and(|at| at < 0)
            || self
                .relative_ms
                .into_iter()
                .chain(self.active_work_ms)
                .any(|ms| ms <= 0 || now_ms.checked_add(ms).is_none())
            || self.expires_at_ms.is_some_and(|at| at <= now_ms)
        {
            return Err("invalid alarm time");
        }
        if let Some(result) = &self.operation_result {
            result.validate()?;
        }
        Ok(())
    }

    pub fn scope_key(&self) -> String {
        // JSON preserves the distinction between absent scope and arbitrary identifiers.
        serde_json::json!([self.project_id, self.workstream_id]).to_string()
    }
}

impl OperationResult {
    pub fn validate(&self) -> Result<(), &'static str> {
        for value in [&self.operation_id, &self.tool_name]
            .into_iter()
            .chain(self.result_code.iter())
        {
            if value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
            {
                return Err("operation triggers require bounded identifiers, not tool output");
            }
        }
        Ok(())
    }

    pub fn matches(&self, result: &Self) -> bool {
        self.operation_id == result.operation_id
            && self.tool_name == result.tool_name
            && self.outcome_class == result.outcome_class
            && self
                .result_code
                .as_ref()
                .is_none_or(|code| result.result_code.as_ref() == Some(code))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Alarm {
    pub id: Uuid,
    pub thread_id: ThreadId,
    pub spec: AlarmSpec,
    pub state: AlarmState,
    pub created_at_ms: i64,
    pub due_at_ms: Option<i64>,
    pub delivered_at_ms: Option<i64>,
    pub acknowledged_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AlarmPage {
    pub data: Vec<Alarm>,
    pub next_offset: Option<usize>,
}

/// Transitions reported by the accounting runtime, including its explicit wait spans.
pub enum AlarmWorkEvent {
    Start { thread_id: ThreadId, eligible: bool },
    Wait,
    Resume,
    Finish,
}
