use super::UsageStatsContext;
use super::storage_error;
use super::tool_error;
use crate::function_tool::FunctionCallError;
use codex_usage::CanonicalRepositoryPath;
use codex_usage::RepositoryId;
use codex_usage::RepositoryIdentityInput;
use codex_usage::RepositoryIdentityMaterial;
use codex_usage::UsagePageRequest;
use codex_usage::UsageStore;

const MAX_REPOSITORY_SCAN: usize = 10_000;

pub(super) async fn optional_repository(
    store: &UsageStore,
    context: &UsageStatsContext,
    reference: Option<&str>,
) -> Result<Option<RepositoryId>, FunctionCallError> {
    match reference {
        Some(reference) => repository(store, context, reference).await.map(Some),
        None => Ok(None),
    }
}

pub(super) async fn repository(
    store: &UsageStore,
    context: &UsageStatsContext,
    reference: &str,
) -> Result<RepositoryId, FunctionCallError> {
    if reference == "current" {
        return current_repository(store, context).await;
    }
    if let Ok(id) = RepositoryId::new(reference)
        && let Some(record) = store
            .read_repository(&id)
            .await
            .map_err(|_| storage_error())?
    {
        return Ok(record.id);
    }
    let mut cursor = None;
    let mut scanned = 0_usize;
    let mut matched = None;
    loop {
        let page = store
            .list_repositories(&UsagePageRequest { cursor, limit: 100 })
            .await
            .map_err(|_| storage_error())?;
        for record in page.data {
            scanned = scanned.checked_add(1).ok_or_else(storage_error)?;
            if scanned > MAX_REPOSITORY_SCAN {
                return Err(tool_error(
                    "repository lookup is too broad; use a repository id",
                ));
            }
            if record.label == reference {
                if matched.is_some() {
                    return Err(tool_error(
                        "repository label is ambiguous; use a repository id",
                    ));
                }
                matched = Some(record.id);
            }
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    matched.ok_or_else(|| tool_error("repository was not found"))
}

pub(super) async fn current_repository(
    store: &UsageStore,
    context: &UsageStatsContext,
) -> Result<RepositoryId, FunctionCallError> {
    let cwd = context
        .cwd
        .clone()
        .ok_or_else(|| tool_error("current repository is unavailable"))?;
    let workspace = codex_git_utils::get_git_repo_root(&cwd).unwrap_or(cwd);
    let workspace = std::fs::canonicalize(&workspace)
        .map_err(|_| tool_error("current repository is unavailable"))?;
    let workspace_text = workspace
        .to_str()
        .ok_or_else(|| tool_error("current repository is unavailable"))?;
    let mut identity = RepositoryIdentityInput::new(
        CanonicalRepositoryPath::new(workspace_text)
            .map_err(|_| tool_error("current repository is unavailable"))?,
    );
    if let Some(common_dir) = codex_usage::discover_git_common_dir(&workspace) {
        identity = identity.with_git_common_dir(common_dir);
    }
    if let Some(origin) = codex_git_utils::collect_git_info(&workspace)
        .await
        .and_then(|info| info.repository_url)
    {
        identity = identity.with_origin(
            RepositoryIdentityMaterial::new(origin)
                .map_err(|_| tool_error("current repository is unavailable"))?,
        );
    }
    store
        .find_repository_for_identity(&identity)
        .await
        .map_err(|_| storage_error())?
        .ok_or_else(|| tool_error("current repository has no collected usage"))
}
