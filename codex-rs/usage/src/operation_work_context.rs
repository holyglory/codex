use crate::UsageStoreError;
use crate::work_binding::valid_binding_identifier;

/// The declaration observed before an operation starts, retained through replay.
/// An empty snapshot records missing context; absence of a snapshot means legacy data.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperationWorkContext {
    pub native_project_id: Option<String>,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
    pub experiment_ref: Option<String>,
}

impl OperationWorkContext {
    pub(crate) fn validate(&self) -> Result<(), UsageStoreError> {
        if self.native_project_id.is_none() && *self != Self::default()
            || [
                &self.native_project_id,
                &self.workstream_id,
                &self.outcome_id,
            ]
            .into_iter()
            .flatten()
            .any(|value| !valid_binding_identifier(value))
        {
            return Err(UsageStoreError::InvalidFact);
        }
        if let Some(reference) = &self.experiment_ref {
            let valid = reference.len() <= 256
                && reference.split_once('@').is_some_and(|(record, revision)| {
                    valid_binding_identifier(record)
                        && revision
                            .parse::<u32>()
                            .is_ok_and(|parsed| parsed > 0 && parsed.to_string() == revision)
                });
            if !valid {
                return Err(UsageStoreError::InvalidFact);
            }
        }
        Ok(())
    }
}
