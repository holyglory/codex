//! Resolve mutable aliases once before a continuation lease begins.
use super::*;

pub(super) fn resolve(
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
) -> Result<UsageFilter, FunctionCallError> {
    // Only summary has a current-chat default. List/detail actions are global
    // unless they carry an explicit thread, repository, or account filter.
    let scope = if matches!(args.action, UsageStatsAction::Summary) {
        args.scope.unwrap_or(UsageStatsScope::CurrentChat)
    } else {
        args.scope.unwrap_or(UsageStatsScope::All)
    };
    let mut filter = UsageFilter::default();
    if let Some(account) = args.account.as_deref() {
        let id = if let Ok(id) = account.parse::<codex_account_registry::AccountId>() {
            id.to_string()
        } else {
            let registry = codex_account_registry::RegistryStore::new(&context.codex_home)
                .read()
                .map_err(|_| tool_error("account registry is unavailable"))?;
            registry
                .accounts
                .iter()
                .find(|profile| profile.alias.as_str() == account)
                .map(|profile| profile.id.to_string())
                .ok_or_else(|| tool_error("unknown account alias"))?
        };
        filter = filter.for_account(id);
    }
    let current_repository = || {
        crate::usage_identity::repository_id(&context.cwd)
            .ok_or_else(|| tool_error("current directory is not a recognized Git repository"))
    };
    match scope {
        UsageStatsScope::All => {}
        UsageStatsScope::CurrentRepository => {
            filter = filter.for_repository(current_repository()?);
        }
        UsageStatsScope::CurrentChat => {
            filter = filter.for_thread(context.thread_id.clone());
        }
        UsageStatsScope::Repository => {
            let repository = args
                .repository
                .as_deref()
                .ok_or_else(|| tool_error("repository is required for repository scope"))?;
            filter = filter.for_repository(if repository == "current" {
                current_repository()?
            } else {
                repository.to_string()
            });
        }
    }
    if let Some(repository) = args.repository.as_deref() {
        filter = filter.for_repository(if repository == "current" {
            current_repository()?
        } else {
            repository.to_string()
        });
    }
    if let Some(thread_id) = args.thread_id.as_deref() {
        filter = filter.for_thread(if thread_id == "current" {
            context.thread_id.clone()
        } else {
            thread_id.to_string()
        });
    }
    Ok(filter)
}
