use super::DEFAULT_PAGE_LIMIT;
use super::MAX_PAGE_LIMIT;
use super::UsageStatsAction;
use super::UsageStatsArgs;
use super::UsageStatsContext;
use super::UsageStatsScope;
use super::query_lists;
use super::repository::current_repository;
use super::repository::optional_repository;
use super::repository::repository;
use super::storage_error;
use super::tool_error;
use crate::function_tool::FunctionCallError;
use codex_account_registry::AccountId;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_usage::AccountProfileRef;
use codex_usage::RepositoryId;
use codex_usage::StructuredTokenAggregate;
use codex_usage::StructuredUsageSummary;
use codex_usage::ThreadId;
use codex_usage::UsageDetailKind;
use codex_usage::UsageDetailListQuery;
use codex_usage::UsagePageCursor;
use codex_usage::UsagePageRequest;
use codex_usage::UsageStore;
use codex_usage::UsageSummaryQuery;
use codex_usage::UtcTimeRange;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;

pub(super) async fn execute(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    validate_args(&args)?;
    let time_range = time_range(args.from_at_ms, args.to_at_ms)?;
    let mut value = match args.action {
        UsageStatsAction::Summary => summary(store, context, &args, time_range).await,
        UsageStatsAction::Repositories => query_lists::repositories(store, &args).await,
        UsageStatsAction::Tools => query_lists::tools(store, context, &args, time_range).await,
        UsageStatsAction::Activities => {
            query_lists::activities(store, context, &args, time_range).await
        }
        UsageStatsAction::Events => query_lists::events(store, context, &args, time_range).await,
        UsageStatsAction::Details => details(store, context, &args, time_range).await,
    }?;
    if let Some(object) = value.as_object_mut() {
        object.insert("reportingOperationInProgress".to_string(), json!(true));
        object.insert(
            "reportingDisclosure".to_string(),
            json!("this usage_stats call is durably started and may appear as capture_started until its result is delivered"),
        );
    }
    Ok(value)
}

async fn summary(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
    time_range: Option<UtcTimeRange>,
) -> Result<Value, FunctionCallError> {
    let (thread_id, repository_id) = summary_scope(store, context, args).await?;
    let account = args
        .account
        .as_deref()
        .map(|reference| resolve_account(context, reference))
        .transpose()?;
    let summary = store
        .usage_summary_query(UsageSummaryQuery {
            thread_id,
            repository_id,
            account_profile_ref: account.as_ref().map(|account| account.profile_ref.clone()),
            time_range,
        })
        .await
        .map_err(|_| storage_error())?;
    let mut structured =
        StructuredUsageSummary::new(&summary, account.map(|account| account.display));
    if matches!(
        args.scope.unwrap_or(UsageStatsScope::CurrentChat),
        UsageStatsScope::All
    ) {
        structured.provider_tokens = aggregate_all_provider_tokens(structured.provider_tokens)?;
    }
    serde_json::to_value(structured).map_err(|_| storage_error())
}

pub(super) fn aggregate_all_provider_tokens(
    tokens: Vec<StructuredTokenAggregate>,
) -> Result<Vec<StructuredTokenAggregate>, FunctionCallError> {
    let mut groups = BTreeMap::<(String, String), StructuredTokenAggregate>::new();
    for token in tokens {
        let aggregate = groups
            .entry((token.category.clone(), token.measurement_provenance.clone()))
            .or_insert_with(|| StructuredTokenAggregate {
                category: token.category.clone(),
                repository_bucket: "all".to_string(),
                measurement_provenance: token.measurement_provenance.clone(),
                measured_tokens: 0,
                exact_tokens: Some(0),
                unknown_observations: 0,
                observation_count: 0,
            });
        aggregate.measured_tokens = aggregate
            .measured_tokens
            .checked_add(token.measured_tokens)
            .ok_or_else(storage_error)?;
        aggregate.exact_tokens = match (aggregate.exact_tokens, token.exact_tokens) {
            (Some(current), Some(value)) => {
                Some(current.checked_add(value).ok_or_else(storage_error)?)
            }
            (None, _) | (_, None) => None,
        };
        aggregate.unknown_observations = aggregate
            .unknown_observations
            .checked_add(token.unknown_observations)
            .ok_or_else(storage_error)?;
        aggregate.observation_count = aggregate
            .observation_count
            .checked_add(token.observation_count)
            .ok_or_else(storage_error)?;
    }
    Ok(groups.into_values().collect())
}

async fn details(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
    time_range: Option<UtcTimeRange>,
) -> Result<Value, FunctionCallError> {
    let detail = args
        .detail
        .as_deref()
        .and_then(UsageDetailKind::parse)
        .ok_or_else(|| tool_error("detail is required and must be a supported kind"))?;
    validate_detail_filters(detail, args)?;
    let account = args
        .account
        .as_deref()
        .map(|reference| resolve_account(context, reference))
        .transpose()?;
    let labels = account_labels(context)?;
    let page = store
        .list_details(
            detail,
            &UsageDetailListQuery {
                page: page(args)?,
                time_range,
                thread_id: optional_thread(context, args.thread_id.as_deref())?,
                repository_id: optional_repository(store, context, args.repository.as_deref())
                    .await?,
                account_profile_ref: account.map(|account| account.profile_ref),
            },
            |reference| account_display(&labels, reference),
        )
        .await
        .map_err(|_| storage_error())?;
    Ok(json!({
        "schemaVersion": 1,
        "kind": "usageDetails",
        "detailKind": detail.as_str(),
        "data": page.data,
        "nextCursor": page.next_cursor.as_ref().map(cursor),
    }))
}

async fn summary_scope(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
) -> Result<(Option<ThreadId>, Option<RepositoryId>), FunctionCallError> {
    match args.scope.unwrap_or(UsageStatsScope::CurrentChat) {
        UsageStatsScope::All => Ok((None, None)),
        UsageStatsScope::CurrentChat => Ok((Some(context.thread_id.clone()), None)),
        UsageStatsScope::CurrentRepository => {
            Ok((None, Some(current_repository(store, context).await?)))
        }
        UsageStatsScope::Repository => {
            let reference = args
                .repository
                .as_deref()
                .ok_or_else(|| tool_error("repository is required for repository scope"))?;
            Ok((None, Some(repository(store, context, reference).await?)))
        }
    }
}

pub(super) fn page(args: &UsageStatsArgs) -> Result<UsagePageRequest, FunctionCallError> {
    let limit = args.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(tool_error("limit must be between 1 and 50"));
    }
    let cursor = match (&args.cursor_sort_value, &args.cursor_id) {
        (None, None) => None,
        (Some(occurred_at_ms), Some(id)) => Some(
            UsagePageCursor::new(*occurred_at_ms, id.clone())
                .ok_or_else(|| tool_error("cursor is invalid"))?,
        ),
        _ => {
            return Err(tool_error(
                "cursor_sort_value and cursor_id must be provided together",
            ));
        }
    };
    Ok(UsagePageRequest { cursor, limit })
}

pub(super) fn cursor(cursor: &UsagePageCursor) -> Value {
    json!({ "sortValue": cursor.occurred_at_ms(), "id": cursor.id() })
}

pub(super) fn optional_thread(
    context: &UsageStatsContext,
    reference: Option<&str>,
) -> Result<Option<ThreadId>, FunctionCallError> {
    reference
        .map(|reference| {
            if reference == "current" {
                Ok(context.thread_id.clone())
            } else {
                ThreadId::new(reference).map_err(|_| tool_error("thread_id is invalid"))
            }
        })
        .transpose()
}

fn time_range(
    start: Option<i64>,
    end: Option<i64>,
) -> Result<Option<UtcTimeRange>, FunctionCallError> {
    if start.is_none() && end.is_none() {
        return Ok(None);
    }
    UtcTimeRange::new(start.unwrap_or(i64::MIN), end.unwrap_or(i64::MAX))
        .map(Some)
        .map_err(|_| tool_error("usage time range is invalid"))
}

struct ResolvedAccount {
    profile_ref: AccountProfileRef,
    display: String,
}

fn resolve_account(
    context: &UsageStatsContext,
    reference: &str,
) -> Result<ResolvedAccount, FunctionCallError> {
    let registry = match RegistryStore::new(&context.codex_home).read() {
        Ok(registry) => Some(registry),
        Err(RegistryStoreError::NotFound) => None,
        Err(_) => return Err(storage_error()),
    };
    if let Some(registry) = registry
        && let Some(account) = account_in_registry(&registry, reference)?
    {
        return Ok(ResolvedAccount {
            profile_ref: AccountProfileRef::new(account.id.as_str())
                .map_err(|_| storage_error())?,
            display: account.alias.as_str().to_string(),
        });
    }
    let id = reference
        .parse::<AccountId>()
        .map_err(|_| tool_error("account was not found"))?;
    let profile_ref = AccountProfileRef::new(id.as_str()).map_err(|_| storage_error())?;
    Ok(ResolvedAccount {
        display: codex_usage::redacted_account_profile_label(&profile_ref),
        profile_ref,
    })
}

fn account_in_registry<'a>(
    registry: &'a AccountRegistry,
    reference: &str,
) -> Result<Option<&'a codex_account_registry::AccountMetadata>, FunctionCallError> {
    let alias_match = registry
        .accounts
        .iter()
        .find(|account| account.alias.as_str() == reference);
    let id_match = reference
        .parse::<AccountId>()
        .ok()
        .and_then(|id| registry.accounts.iter().find(|account| account.id == id));
    match (alias_match, id_match) {
        (Some(alias), Some(id)) if alias.id != id.id => {
            Err(tool_error("account reference is ambiguous"))
        }
        (Some(account), _) | (_, Some(account)) => Ok(Some(account)),
        (None, None) => Ok(None),
    }
}

fn account_labels(
    context: &UsageStatsContext,
) -> Result<HashMap<String, String>, FunctionCallError> {
    match RegistryStore::new(&context.codex_home).read() {
        Ok(registry) => Ok(registry
            .accounts
            .into_iter()
            .map(|account| (account.id.to_string(), account.alias.to_string()))
            .collect()),
        Err(RegistryStoreError::NotFound) => Ok(HashMap::new()),
        Err(_) => Err(storage_error()),
    }
}

fn account_display(labels: &HashMap<String, String>, reference: &AccountProfileRef) -> String {
    labels
        .get(reference.as_str())
        .cloned()
        .unwrap_or_else(|| codex_usage::redacted_account_profile_label(reference))
}

fn validate_args(args: &UsageStatsArgs) -> Result<(), FunctionCallError> {
    let invalid = match args.action {
        UsageStatsAction::Summary => {
            args.thread_id.is_some()
                || args.agent_id.is_some()
                || args.detail.is_some()
                || args.limit.is_some()
                || args.cursor_sort_value.is_some()
                || args.cursor_id.is_some()
                || (args.repository.is_some()
                    && !matches!(args.scope, Some(UsageStatsScope::Repository)))
        }
        UsageStatsAction::Repositories => {
            args.scope.is_some()
                || args.repository.is_some()
                || args.account.is_some()
                || args.thread_id.is_some()
                || args.agent_id.is_some()
                || args.detail.is_some()
                || args.from_at_ms.is_some()
                || args.to_at_ms.is_some()
        }
        UsageStatsAction::Tools | UsageStatsAction::Events => {
            args.scope.is_some()
                || args.account.is_some()
                || args.agent_id.is_some()
                || args.detail.is_some()
        }
        UsageStatsAction::Activities => {
            args.scope.is_some()
                || args.repository.is_some()
                || args.account.is_some()
                || args.detail.is_some()
        }
        UsageStatsAction::Details => args.scope.is_some() || args.agent_id.is_some(),
    };
    if invalid {
        Err(tool_error(
            "usage_stats fields do not apply to the selected action",
        ))
    } else {
        Ok(())
    }
}

fn validate_detail_filters(
    detail: UsageDetailKind,
    args: &UsageStatsArgs,
) -> Result<(), FunctionCallError> {
    let invalid = match detail {
        UsageDetailKind::Processes => {
            args.repository.is_some() || args.account.is_some() || args.thread_id.is_some()
        }
        UsageDetailKind::Threads | UsageDetailKind::Agents | UsageDetailKind::LifecycleEvents => {
            args.repository.is_some() || args.account.is_some()
        }
        UsageDetailKind::Turns => args.repository.is_some(),
        UsageDetailKind::Operations
        | UsageDetailKind::Tokens
        | UsageDetailKind::Approvals
        | UsageDetailKind::RepositoryAttributions
        | UsageDetailKind::Classifications
        | UsageDetailKind::Coverage
        | UsageDetailKind::ActivitySpans => false,
        UsageDetailKind::RepositoryIdentities | UsageDetailKind::RepositoryEvents => {
            args.account.is_some() || args.thread_id.is_some()
        }
        UsageDetailKind::Taxonomies => {
            args.repository.is_some()
                || args.account.is_some()
                || args.thread_id.is_some()
                || args.from_at_ms.is_some()
                || args.to_at_ms.is_some()
        }
    };
    if invalid {
        Err(tool_error(
            "filters do not apply to the selected detail kind",
        ))
    } else {
        Ok(())
    }
}
