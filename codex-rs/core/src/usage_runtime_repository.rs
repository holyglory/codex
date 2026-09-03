use super::*;
use codex_usage::CanonicalRepositoryPath;
use codex_usage::NewRepositoryAttribution;
use codex_usage::RepositoryAttributionKind;
use codex_usage::RepositoryAttributionProvenance;
use codex_usage::RepositoryId;
use codex_usage::RepositoryIdentityInput;
use codex_usage::RepositoryIdentityMaterial;
use codex_usage::SafeRepositoryLabel;
use std::path::Path;

/// Raw repository material owned only until the usage boundary hashes it.
///
/// Deliberately does not implement `Debug` or `Clone`.
pub(crate) struct RepositoryCandidate {
    workspace: String,
    origin: Option<String>,
    common_dir: Option<String>,
    safe_label: String,
}

pub(crate) fn repository_safe_label(workspace: &str) -> String {
    let candidate = workspace
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let label = candidate
        .chars()
        .filter(|character| character.is_alphanumeric() || " -_.".contains(*character))
        .take(80)
        .collect::<String>();
    if label.trim().is_empty() {
        "repository".to_string()
    } else {
        label
    }
}

impl RepositoryCandidate {
    pub(crate) fn new(
        workspace: impl Into<String>,
        origin: Option<String>,
        safe_label: impl Into<String>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            origin,
            common_dir: None,
            safe_label: safe_label.into(),
        }
    }
}

#[derive(Default)]
pub(super) struct TurnRepositoryState {
    inner: Mutex<TurnRepositoryEntries>,
}

#[derive(Default)]
struct TurnRepositoryEntries {
    ids: HashMap<String, Vec<RepositoryId>>,
    workspace_choices: HashMap<String, HashMap<RepositoryId, RepositoryChoice>>,
}

#[derive(Clone)]
struct RepositoryChoice {
    id: RepositoryId,
    authority: u8,
}

pub(super) struct RepositoryResolution {
    pub(super) ids: Vec<RepositoryId>,
    pub(super) bucket: RepositoryBucket,
}

impl UsageRuntime {
    pub(super) async fn resolve_repositories(
        &self,
        store: &UsageStore,
        thread_id: &str,
        turn_id: Option<&str>,
        candidates: &[RepositoryCandidate],
    ) -> Result<RepositoryResolution, CodexErr> {
        let key = tool::activity_key(thread_id, turn_id);
        let mut resolved = Vec::new();
        for candidate in candidates {
            let workspace = canonical_path(&candidate.workspace)
                .ok_or_else(|| self.reject_invalid_metadata())?;
            let workspace_key = self.write_required(
                store.repository_id_for_identity(&RepositoryIdentityInput::new(
                    canonical_path(&candidate.workspace)
                        .ok_or_else(|| self.reject_invalid_metadata())?,
                )),
            )?;
            let discovered_common = match &candidate.common_dir {
                Some(common_dir) => {
                    Some(canonical_path(common_dir).ok_or_else(|| self.reject_invalid_metadata())?)
                }
                None => codex_usage::discover_git_common_dir(Path::new(&candidate.workspace)),
            };
            let authority = if candidate.origin.is_some() {
                3
            } else if discovered_common.is_some() {
                2
            } else {
                1
            };
            let mut identity = RepositoryIdentityInput::new(workspace);
            if let Some(common_dir) = discovered_common {
                identity = identity.with_git_common_dir(common_dir);
            }
            if let Some(origin) = &candidate.origin {
                identity = identity.with_origin(
                    RepositoryIdentityMaterial::new(origin.as_str())
                        .map_err(|_| self.reject_invalid_metadata())?,
                );
            }
            let label = SafeRepositoryLabel::new(candidate.safe_label.as_str())
                .map_err(|_| self.reject_invalid_metadata())?;
            let repository_id =
                self.write_required(store.resolve_repository(&identity, &label, now_ms()).await)?;
            resolved.push((
                workspace_key,
                RepositoryChoice {
                    id: repository_id,
                    authority,
                },
            ));
        }

        let mut merges = Vec::new();
        let ids = {
            let mut state = self.repository_state.inner.lock().await;
            let TurnRepositoryEntries {
                ids,
                workspace_choices,
            } = &mut *state;
            let choices = workspace_choices.entry(key.clone()).or_default();
            let ids = ids.entry(key).or_default();
            for (workspace_key, candidate) in resolved {
                let selected = match choices.get(&workspace_key) {
                    Some(previous) if previous.id == candidate.id => previous.clone(),
                    Some(previous) if candidate.authority > previous.authority => {
                        merges.push((previous.id.clone(), candidate.id.clone()));
                        ids.retain(|repository_id| repository_id != &previous.id);
                        choices.insert(workspace_key, candidate.clone());
                        candidate
                    }
                    Some(previous) if candidate.authority < previous.authority => previous.clone(),
                    Some(_) => candidate,
                    None => {
                        choices.insert(workspace_key, candidate.clone());
                        candidate
                    }
                };
                if !ids.contains(&selected.id) {
                    ids.push(selected.id);
                }
            }
            ids.clone()
        };
        for (previous_id, repository_id) in merges {
            self.write_required(
                store
                    .append_repository_merge(
                        FactEventId::new(),
                        &previous_id,
                        &repository_id,
                        now_ms(),
                    )
                    .await,
            )?;
        }
        let bucket = match ids.as_slice() {
            [] => RepositoryBucket::Unknown,
            [repository_id] => RepositoryBucket::Single(repository_id.clone()),
            [_, _, ..] => RepositoryBucket::MultiRepo,
        };
        Ok(RepositoryResolution { ids, bucket })
    }

    pub(super) async fn record_repository_resolution(
        &self,
        store: &UsageStore,
        operation_id: OperationId,
        resolution: &RepositoryResolution,
        occurred_at_ms: i64,
    ) -> Result<(), CodexErr> {
        if resolution.ids.is_empty() {
            return self.write_required_for(
                operation_id,
                store
                    .record_repository_attribution(&NewRepositoryAttribution {
                        event_id: FactEventId::new(),
                        operation_id,
                        repository_id: None,
                        kind: RepositoryAttributionKind::Unknown,
                        provenance: RepositoryAttributionProvenance::Unknown,
                        occurred_at_ms,
                    })
                    .await,
            );
        }
        for (index, repository_id) in resolution.ids.iter().enumerate() {
            self.write_required_for(
                operation_id,
                store
                    .record_repository_attribution(&NewRepositoryAttribution {
                        event_id: FactEventId::new(),
                        operation_id,
                        repository_id: Some(repository_id.clone()),
                        kind: if index == 0 {
                            RepositoryAttributionKind::Primary
                        } else {
                            RepositoryAttributionKind::ObservedCwd
                        },
                        provenance: RepositoryAttributionProvenance::RuntimeObserved,
                        occurred_at_ms,
                    })
                    .await,
            )?;
        }
        if resolution.ids.len() > 1 {
            self.write_required_for(
                operation_id,
                store
                    .record_repository_attribution(&NewRepositoryAttribution {
                        event_id: FactEventId::new(),
                        operation_id,
                        repository_id: None,
                        kind: RepositoryAttributionKind::MultiRepo,
                        provenance: RepositoryAttributionProvenance::RuntimeObserved,
                        occurred_at_ms,
                    })
                    .await,
            )?;
        }
        Ok(())
    }
}

fn canonical_path(value: &str) -> Option<CanonicalRepositoryPath> {
    let native = Path::new(value);
    let canonical = native
        .is_absolute()
        .then(|| std::fs::canonicalize(native).ok())
        .flatten()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| value.to_string());
    CanonicalRepositoryPath::new(canonical).ok()
}
