use crate::FactEventId;
use crate::ThreadId;
use crate::UsageStore;
use crate::UsageStoreError;
use sqlx::Row;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewWorkBinding {
    pub thread_id: ThreadId,
    pub native_project_id: String,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
    pub experiment_ref: Option<String>,
    pub observed_at_ms: i64,
}

impl UsageStore {
    pub async fn record_work_binding(
        &self,
        binding: &NewWorkBinding,
    ) -> Result<(), UsageStoreError> {
        validate(binding)?;
        let identity = format!(
            "work_binding_v1|{}|{}|{}|{}|{}|{}",
            binding.thread_id.as_str(),
            binding.native_project_id,
            binding.workstream_id.as_deref().unwrap_or_default(),
            binding.outcome_id.as_deref().unwrap_or_default(),
            binding.experiment_ref.as_deref().unwrap_or_default(),
            binding.observed_at_ms
        );
        let event_id = FactEventId::from_stable_key(identity.as_bytes()).as_string();
        let result = sqlx::query("INSERT INTO work_bindings (event_id, thread_id, native_project_id, workstream_id,
            outcome_id, experiment_ref, observed_at_ms, provenance) VALUES (?, ?, ?, ?, ?, ?, ?, 'runtime_observed')
            ON CONFLICT(event_id) DO NOTHING")
            .bind(&event_id).bind(binding.thread_id.as_str()).bind(&binding.native_project_id)
            .bind(&binding.workstream_id).bind(&binding.outcome_id).bind(&binding.experiment_ref)
            .bind(binding.observed_at_ms).execute(&self.pool).await.map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 {
            let row = sqlx::query("SELECT * FROM work_bindings WHERE event_id = ?")
                .bind(event_id)
                .fetch_one(&self.pool)
                .await
                .map_err(UsageStoreError::Database)?;
            if from_row(&row)? != *binding
                || row
                    .try_get::<String, _>("provenance")
                    .map_err(UsageStoreError::Database)?
                    != "runtime_observed"
            {
                return Err(UsageStoreError::FactConflict);
            }
        }
        Ok(())
    }
}

fn valid_binding_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn validate(binding: &NewWorkBinding) -> Result<(), UsageStoreError> {
    if !valid_binding_identifier(&binding.native_project_id)
        || [&binding.workstream_id, &binding.outcome_id]
            .into_iter()
            .flatten()
            .any(|value| !valid_binding_identifier(value))
    {
        return Err(UsageStoreError::InvalidFact);
    }
    if let Some(reference) = &binding.experiment_ref {
        if reference.len() > 256 {
            return Err(UsageStoreError::InvalidFact);
        }
        let (record, revision) = reference
            .split_once('@')
            .ok_or(UsageStoreError::InvalidFact)?;
        let parsed = revision
            .parse::<u32>()
            .map_err(|_| UsageStoreError::InvalidFact)?;
        if !valid_binding_identifier(record) || parsed == 0 || parsed.to_string() != revision {
            return Err(UsageStoreError::InvalidFact);
        }
    }
    Ok(())
}

pub(crate) fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<NewWorkBinding, UsageStoreError> {
    let binding = NewWorkBinding {
        thread_id: ThreadId::new(
            row.try_get::<String, _>("thread_id")
                .map_err(UsageStoreError::Database)?,
        )
        .map_err(|_| UsageStoreError::InvalidFact)?,
        native_project_id: row
            .try_get("native_project_id")
            .map_err(UsageStoreError::Database)?,
        workstream_id: row
            .try_get("workstream_id")
            .map_err(UsageStoreError::Database)?,
        outcome_id: row
            .try_get("outcome_id")
            .map_err(UsageStoreError::Database)?,
        experiment_ref: row
            .try_get("experiment_ref")
            .map_err(UsageStoreError::Database)?,
        observed_at_ms: row
            .try_get("observed_at_ms")
            .map_err(UsageStoreError::Database)?,
    };
    validate(&binding)?;
    Ok(binding)
}
