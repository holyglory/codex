use clap::Args;
use clap::Parser;
use clap::ValueEnum;
use codex_account_registry::AccountId;
use codex_account_registry::AccountRegistry;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_core::config::ConfigBuilder;
use codex_usage::AccountProfileRef;
use codex_usage::AgentId;
use codex_usage::CanonicalRepositoryPath;
use codex_usage::FactEventId;
use codex_usage::RepositoryId;
use codex_usage::RepositoryIdentityInput;
use codex_usage::RepositoryIdentityMaterial;
use codex_usage::SafeRepositoryLabel;
use codex_usage::TerminalStatus;
use codex_usage::ThreadId;
use codex_usage::UsageActivityListQuery;
use codex_usage::UsageDetailKind;
use codex_usage::UsageDetailListQuery;
use codex_usage::UsageEventListQuery;
use codex_usage::UsagePageRequest;
use codex_usage::UsageStore;
use codex_usage::UsageSummaryQuery;
use codex_usage::UsageToolListQuery;
use codex_usage::UtcTimeRange;
use codex_utils_cli::CliConfigOverrides;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

mod error;
mod export;
mod filter;
mod render;

pub(crate) use error::UsageCommandError;
pub(crate) use error::print_error;
use filter::PageArgs;
use filter::UsageFilters;
use filter::breakdown_activities;
use filter::breakdown_events;
use filter::breakdown_tools;
use filter::combine_fixed;
#[cfg(test)]
use filter::decode_cursor;
use filter::encode_cursor;
use filter::page_request;

const DEFAULT_PAGE_LIMIT: u32 = 100;
const MAX_REPOSITORY_SCAN: usize = 10_000;

#[derive(Debug, Parser)]
pub(crate) struct UsageCommand {
    #[clap(skip)]
    pub(crate) config_overrides: CliConfigOverrides,

    /// Emit stable, versioned JSON.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(flatten)]
    filters: UsageFilters,

    #[command(subcommand)]
    action: UsageAction,
}

#[derive(Debug, clap::Subcommand)]
enum UsageAction {
    /// Summarize all matching usage.
    Summary,
    /// Summarize one chat.
    Chat(ChatArgs),
    /// Summarize or manage one repository.
    Repo(RepoArgs),
    /// List repositories known to this local collector.
    Repositories(PageArgs),
    /// List tool invocations.
    Tools(PageArgs),
    /// List operation activities.
    Activities(PageArgs),
    /// List content-free collector events.
    Events(PageArgs),
    /// List a complete content-free collector record family.
    Details(DetailsArgs),
    /// Correct one operation's effective classification.
    Classify(ClassifyArgs),
    /// Export content-free usage events to a new private file.
    Export(ExportArgs),
    /// Validate usage storage and report incomplete operations.
    Doctor,
}

#[derive(Debug, Args)]
struct ChatArgs {
    #[arg(value_name = "THREAD_ID")]
    thread_id: String,
}

#[derive(Debug, Args)]
struct RepoArgs {
    #[command(subcommand)]
    action: Option<RepoAction>,
    #[arg(value_name = "REPO_OR_CURRENT")]
    reference: Option<String>,
    /// Resolve the stored repository identity without aggregating usage history.
    #[arg(long)]
    identity_only: bool,
}

#[derive(Debug, clap::Subcommand)]
enum RepoAction {
    /// Append a safe display alias.
    Alias(RepoAliasArgs),
    /// Merge one repository identity into another.
    Merge(RepoMergeArgs),
}

#[derive(Debug, Args)]
struct RepoAliasArgs {
    #[arg(value_name = "REPOSITORY")]
    repository: String,
    #[arg(value_name = "ALIAS")]
    alias: String,
}

#[derive(Debug, Args)]
struct RepoMergeArgs {
    #[arg(value_name = "SOURCE")]
    source: String,
    #[arg(value_name = "TARGET")]
    target: String,
}

#[derive(Debug, Args)]
struct ClassifyArgs {
    #[arg(value_name = "OPERATION")]
    operation: String,
}

#[derive(Debug, Args)]
struct DetailsArgs {
    #[arg(value_name = "KIND")]
    kind: String,
    #[command(flatten)]
    page: PageArgs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ExportFormat {
    Jsonl,
    Csv,
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[arg(long, value_name = "PATH")]
    output: PathBuf,
    #[arg(long, value_enum)]
    format: ExportFormat,
}

pub(crate) async fn run(
    command: UsageCommand,
    strict_config: bool,
) -> Result<(), UsageCommandError> {
    let overrides = command
        .config_overrides
        .parse_overrides()
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Configuration))?;
    let config = ConfigBuilder::default()
        .cli_overrides(overrides)
        .strict_config(strict_config)
        .build()
        .await
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Configuration))?;
    let store = UsageStore::open(&config.codex_home).await?;
    let registry_store = RegistryStore::new(&config.codex_home);
    execute(
        &store,
        &registry_store,
        command.action,
        command.filters,
        command.json,
    )
    .await
}

async fn execute(
    store: &UsageStore,
    registry_store: &RegistryStore,
    action: UsageAction,
    filters: UsageFilters,
    json_output: bool,
) -> Result<(), UsageCommandError> {
    match action {
        UsageAction::Summary => {
            let (summary, account) = build_summary(
                store,
                registry_store,
                &filters,
                /*fixed_thread*/ None,
                /*fixed_repository*/ None,
            )
            .await?;
            println!(
                "{}",
                render::summary(&summary, json_output, account.as_deref())?
            );
        }
        UsageAction::Chat(args) => {
            let thread_id = ThreadId::new(args.thread_id)
                .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Input))?;
            if store.read_thread(&thread_id).await?.is_none() {
                return Err(UsageCommandError::new(error::UsageErrorKind::NotFound));
            }
            let (summary, account) = build_summary(
                store,
                registry_store,
                &filters,
                Some(thread_id),
                /*fixed_repository*/ None,
            )
            .await?;
            println!(
                "{}",
                render::summary(&summary, json_output, account.as_deref())?
            );
        }
        UsageAction::Repo(args) if args.identity_only && args.action.is_some() => {
            return Err(UsageCommandError::new(error::UsageErrorKind::Input));
        }
        UsageAction::Repo(args) => match args.action {
            None => {
                let repository = resolve_repository(store, args.reference.as_deref()).await?;
                if args.identity_only {
                    filters.ensure_only(&[], &[])?;
                    if json_output {
                        println!(
                            "{}",
                            json!({
                                "schemaVersion": 1,
                                "kind": "usageRepositoryIdentity",
                                "databaseSchemaVersion": store.database_schema_version().await?,
                                "taxonomyVersion": codex_usage::TAXONOMY_VERSION,
                                "scope": { "type": "repository", "id": repository.as_str() }
                            })
                        );
                    } else {
                        println!("{}", repository.as_str());
                    }
                    return Ok(());
                }
                let (summary, account) = build_summary(
                    store,
                    registry_store,
                    &filters,
                    /*fixed_thread*/ None,
                    Some(repository),
                )
                .await?;
                println!(
                    "{}",
                    render::summary(&summary, json_output, account.as_deref())?
                );
            }
            Some(RepoAction::Alias(args)) => {
                filters.ensure_only(&[], &[])?;
                let repository = resolve_repository(store, Some(&args.repository)).await?;
                let alias = SafeRepositoryLabel::new(args.alias)
                    .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Input))?;
                store
                    .append_repository_alias(FactEventId::new(), &repository, &alias, now_ms())
                    .await?;
                print_mutation(json_output, "repositoryAliasAdded", repository.as_str());
            }
            Some(RepoAction::Merge(args)) => {
                filters.ensure_only(&[], &[])?;
                let source = resolve_repository(store, Some(&args.source)).await?;
                let target = resolve_repository(store, Some(&args.target)).await?;
                if source == target {
                    return Err(UsageCommandError::new(error::UsageErrorKind::Input));
                }
                store
                    .append_repository_merge(FactEventId::new(), &source, &target, now_ms())
                    .await?;
                print_mutation(json_output, "repositoryMerged", target.as_str());
            }
        },
        UsageAction::Repositories(page) => {
            list_repositories(store, &filters, page, json_output).await?
        }
        UsageAction::Tools(page) => list_tools(store, &filters, page, json_output).await?,
        UsageAction::Activities(page) => {
            list_activities(store, &filters, page, json_output).await?
        }
        UsageAction::Events(page) => list_events(store, &filters, page, json_output).await?,
        UsageAction::Details(args) => {
            list_details(store, registry_store, &filters, args, json_output).await?
        }
        UsageAction::Classify(args) => {
            filters.ensure_only(&["phase", "activity"], &[])?;
            let phase = filters
                .phase_value()?
                .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::Input))?;
            let activity = filters
                .activity_value()?
                .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::Input))?;
            let target = FactEventId::from_string(&args.operation)
                .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::Input))?;
            let event = store
                .correct_classification(target, phase, activity, now_ms())
                .await?
                .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::NotFound))?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "schemaVersion": 1,
                        "kind": "classificationCorrected",
                        "eventId": event.id.as_string(),
                        "phase": phase.as_str(),
                        "activity": activity.as_str(),
                        "provenance": event.provenance.as_str(),
                    }))
                    .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Storage))?
                );
            } else {
                println!("Classification corrected.");
            }
        }
        UsageAction::Export(args) => {
            filters.ensure_only(
                &[
                    "repository",
                    "thread",
                    "provenance",
                    "coverage",
                    "since",
                    "until",
                ],
                &[],
            )?;
            let time_range = filters.time_range()?;
            let repository_id =
                resolve_optional_repository(store, filters.repository.as_deref()).await?;
            let thread_id = filters.thread_id()?;
            let count = export::write(
                store,
                &args.output,
                args.format,
                UsageEventListQuery {
                    page: UsagePageRequest {
                        cursor: None,
                        limit: DEFAULT_PAGE_LIMIT,
                    },
                    time_range,
                    thread_id,
                    repository_id,
                    kind: None,
                },
                filters.event_provenance()?,
                filters.coverage_state()?,
            )
            .await?;
            if json_output {
                println!(
                    "{}",
                    json!({ "schemaVersion": 1, "kind": "usageExport", "records": count })
                );
            } else {
                println!("Exported {count} usage events.");
            }
        }
        UsageAction::Doctor => {
            filters.ensure_only(&[], &[])?;
            let report = store.doctor().await?;
            if report.integrity != "ok" {
                return Err(UsageCommandError::new(error::UsageErrorKind::Corrupt));
            }
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "schemaVersion": 1,
                        "kind": "usageDoctor",
                        "integrity": report.integrity,
                        "migrationCount": report.migration_count,
                        "incompleteOperations": report.incomplete_operations,
                    }))
                    .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Storage))?
                );
            } else {
                println!("Usage storage: ok");
                println!("Migrations: {}", report.migration_count);
                println!("Incomplete operations: {}", report.incomplete_operations);
            }
        }
    }
    Ok(())
}

async fn list_details(
    store: &UsageStore,
    registry_store: &RegistryStore,
    filters: &UsageFilters,
    args: DetailsArgs,
    json_output: bool,
) -> Result<(), UsageCommandError> {
    let kind = UsageDetailKind::parse(&args.kind)
        .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::Input))?;
    let allowed = match kind {
        UsageDetailKind::Processes => &["since", "until"][..],
        UsageDetailKind::Threads | UsageDetailKind::Agents | UsageDetailKind::LifecycleEvents => {
            &["thread", "since", "until"][..]
        }
        UsageDetailKind::Turns => &["account", "thread", "since", "until"][..],
        UsageDetailKind::Operations
        | UsageDetailKind::Tokens
        | UsageDetailKind::Approvals
        | UsageDetailKind::RepositoryAttributions
        | UsageDetailKind::Classifications
        | UsageDetailKind::Coverage
        | UsageDetailKind::ActivitySpans => {
            &["account", "repository", "thread", "since", "until"][..]
        }
        UsageDetailKind::RepositoryIdentities | UsageDetailKind::RepositoryEvents => {
            &["repository", "since", "until"][..]
        }
        UsageDetailKind::Taxonomies => &[],
    };
    filters.ensure_only(allowed, &[])?;
    let cursor_kind = filters.cursor_kind(&format!("details-{}", kind.as_str()));
    let account = filters
        .account
        .as_deref()
        .map(|reference| resolve_account_filter(registry_store, reference))
        .transpose()?;
    let labels = account_labels(registry_store)?;
    let page = store
        .list_details(
            kind,
            &UsageDetailListQuery {
                page: page_request(&args.page, &cursor_kind)?,
                time_range: filters.time_range()?,
                thread_id: filters.thread_id()?,
                repository_id: resolve_optional_repository(store, filters.repository.as_deref())
                    .await?,
                account_profile_ref: account.map(|account| account.profile_ref),
            },
            |reference| account_display(&labels, reference),
        )
        .await?;
    if json_output {
        println!(
            "{}",
            json!({
                "schemaVersion": 1,
                "kind": "usageDetails",
                "detailKind": kind.as_str(),
                "data": page.data,
                "nextCursor": page.next_cursor.as_ref().map(|cursor| encode_cursor(&cursor_kind, cursor)),
            })
        );
    } else {
        println!("Usage details: {}", kind.as_str());
        if page.data.is_empty() {
            println!("  none observed");
        }
        for record in &page.data {
            println!(
                "{}",
                serde_json::to_string(record)
                    .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Storage))?
            );
        }
        if let Some(cursor) = &page.next_cursor {
            println!("Next cursor: {}", encode_cursor(&cursor_kind, cursor));
        }
    }
    Ok(())
}

async fn list_repositories(
    store: &UsageStore,
    filters: &UsageFilters,
    page: PageArgs,
    json_output: bool,
) -> Result<(), UsageCommandError> {
    filters.ensure_only(&[], &[])?;
    let cursor_kind = filters.cursor_kind("repositories");
    let result = store
        .list_repositories(&page_request(&page, &cursor_kind)?)
        .await?;
    if json_output {
        println!(
            "{}",
            json!({
                "schemaVersion": 1,
                "kind": "usageRepositories",
                "data": result.data.iter().map(|record| json!({
                    "id": record.id.as_str(),
                    "label": record.label,
                    "createdAtMs": record.created_at_ms,
                    "updatedAtMs": record.updated_at_ms,
                })).collect::<Vec<_>>(),
                "nextCursor": result.next_cursor.as_ref().map(|cursor| encode_cursor(&cursor_kind, cursor)),
            })
        );
    } else {
        println!("Usage repositories:");
        if result.data.is_empty() {
            println!("  none observed");
        }
        for record in &result.data {
            println!("{}  {}", record.id.as_str(), record.label);
        }
        if let Some(cursor) = &result.next_cursor {
            println!("Next cursor: {}", encode_cursor(&cursor_kind, cursor));
        }
    }
    Ok(())
}

async fn build_summary(
    store: &UsageStore,
    registry_store: &RegistryStore,
    filters: &UsageFilters,
    fixed_thread: Option<ThreadId>,
    fixed_repository: Option<RepositoryId>,
) -> Result<(codex_usage::UsageSummary, Option<String>), UsageCommandError> {
    filters.ensure_only(&["account", "repository", "thread", "since", "until"], &[])?;
    let filter_thread = filters.thread_id()?;
    let thread_id = combine_fixed(fixed_thread, filter_thread)?;
    let filter_repository =
        resolve_optional_repository(store, filters.repository.as_deref()).await?;
    let repository_id = combine_fixed(fixed_repository, filter_repository)?;
    let account = filters
        .account
        .as_deref()
        .map(|reference| resolve_account_filter(registry_store, reference))
        .transpose()?;
    let summary = store
        .usage_summary_query(UsageSummaryQuery {
            thread_id,
            repository_id,
            account_profile_ref: account.as_ref().map(|account| account.profile_ref.clone()),
            time_range: filters.time_range()?,
        })
        .await
        .map_err(UsageCommandError::from)?;
    Ok((summary, account.map(|account| account.display)))
}

struct ResolvedAccountFilter {
    profile_ref: AccountProfileRef,
    display: String,
}

fn resolve_account_filter(
    store: &RegistryStore,
    reference: &str,
) -> Result<ResolvedAccountFilter, UsageCommandError> {
    let registry = match store.read() {
        Ok(registry) => Some(registry),
        Err(RegistryStoreError::NotFound) => None,
        Err(_) => {
            return Err(UsageCommandError::new(error::UsageErrorKind::Storage));
        }
    };
    if let Some(registry) = registry
        && let Some(account) = account_in_registry(&registry, reference)?
    {
        return Ok(ResolvedAccountFilter {
            profile_ref: AccountProfileRef::new(account.id.as_str())
                .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Storage))?,
            display: account.alias.as_str().to_string(),
        });
    }
    let id = reference
        .parse::<AccountId>()
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::NotFound))?;
    let profile_ref = AccountProfileRef::new(id.as_str())
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Input))?;
    Ok(ResolvedAccountFilter {
        display: codex_usage::redacted_account_profile_label(&profile_ref),
        profile_ref,
    })
}

fn account_in_registry<'a>(
    registry: &'a AccountRegistry,
    reference: &str,
) -> Result<Option<&'a codex_account_registry::AccountMetadata>, UsageCommandError> {
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
            Err(UsageCommandError::new(error::UsageErrorKind::Input))
        }
        (Some(account), _) | (_, Some(account)) => Ok(Some(account)),
        (None, None) => Ok(None),
    }
}

fn account_labels(store: &RegistryStore) -> Result<HashMap<String, String>, UsageCommandError> {
    match store.read() {
        Ok(registry) => Ok(registry
            .accounts
            .into_iter()
            .map(|account| (account.id.to_string(), account.alias.to_string()))
            .collect()),
        Err(RegistryStoreError::NotFound) => Ok(HashMap::new()),
        Err(_) => Err(UsageCommandError::new(error::UsageErrorKind::Storage)),
    }
}

fn account_display(labels: &HashMap<String, String>, reference: &AccountProfileRef) -> String {
    labels
        .get(reference.as_str())
        .cloned()
        .unwrap_or_else(|| codex_usage::redacted_account_profile_label(reference))
}

async fn list_tools(
    store: &UsageStore,
    filters: &UsageFilters,
    page: PageArgs,
    json_output: bool,
) -> Result<(), UsageCommandError> {
    filters.ensure_only(
        &[
            "repository",
            "thread",
            "tool",
            "status",
            "provenance",
            "since",
            "until",
        ],
        &["repository", "thread", "tool", "status", "provenance"],
    )?;
    let repository_id = resolve_optional_repository(store, filters.repository.as_deref()).await?;
    let thread_id = filters.thread_id()?;
    let cursor_kind = filters.cursor_kind("tools");
    let result = store
        .list_tools(&UsageToolListQuery {
            page: page_request(&page, &cursor_kind)?,
            time_range: filters.time_range()?,
            thread_id: thread_id.clone(),
            repository_id: repository_id.clone(),
        })
        .await?;
    let status = filters.terminal_status()?;
    let provenance = filters.attribution_provenance()?;
    let data = result
        .data
        .into_iter()
        .filter(|record| {
            filters
                .tool
                .as_deref()
                .is_none_or(|expected| record.tool_name.as_str() == expected)
                && status.is_none_or(|expected| record.status == Some(expected))
                && provenance.is_none_or(|expected| record.provenance == expected)
        })
        .collect::<Vec<_>>();
    let coverage = coverage_header(store, thread_id, repository_id, filters.time_range()?).await?;
    if json_output {
        println!(
            "{}",
            json!({
                "schemaVersion": 1,
                "kind": "usageTools",
                "coverage": coverage,
                "data": data.iter().map(|record| json!({
                    "id": record.id.as_string(),
                    "threadId": record.thread_id.as_str(),
                    "repositoryId": record.repository_id.as_ref().map(RepositoryId::as_str),
                    "tool": record.tool_name.as_str(),
                    "family": record.operation_family.as_str(),
                    "startedAtMs": record.started_at_ms,
                    "completedAtMs": record.completed_at_ms,
                    "status": record.status.map(TerminalStatus::as_str),
                    "provenance": record.provenance.as_str(),
                })).collect::<Vec<_>>(),
                "nextCursor": result.next_cursor.as_ref().map(|cursor| encode_cursor(&cursor_kind, cursor)),
                "breakdown": breakdown_tools(&data, &filters.breakdown),
            })
        );
    } else {
        println!("Usage scope: tools");
        println!("Coverage: {coverage}");
        for record in &data {
            println!(
                "{} {} {} {}",
                record.started_at_ms,
                record.tool_name.as_str(),
                record.status.map_or("unknown", TerminalStatus::as_str),
                record.provenance.as_str()
            );
        }
        if let Some(cursor) = &result.next_cursor {
            println!("Next cursor: {}", encode_cursor(&cursor_kind, cursor));
        }
    }
    Ok(())
}

async fn list_activities(
    store: &UsageStore,
    filters: &UsageFilters,
    page: PageArgs,
    json_output: bool,
) -> Result<(), UsageCommandError> {
    filters.ensure_only(
        &[
            "agent",
            "phase",
            "activity",
            "thread",
            "provenance",
            "since",
            "until",
        ],
        &["agent", "phase", "activity", "thread", "provenance"],
    )?;
    let thread_id = filters.thread_id()?;
    let cursor_kind = filters.cursor_kind("activities");
    let agent_id = filters
        .agent
        .as_ref()
        .map(AgentId::new)
        .transpose()
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Input))?;
    let result = store
        .list_activities(&UsageActivityListQuery {
            page: page_request(&page, &cursor_kind)?,
            time_range: filters.time_range()?,
            thread_id: thread_id.clone(),
            agent_id,
        })
        .await?;
    let phase = filters.phase_value()?;
    let activity = filters.activity_value()?;
    let provenance = filters.attribution_provenance()?;
    let data = result
        .data
        .into_iter()
        .filter(|record| {
            phase.is_none_or(|expected| record.phase == expected)
                && activity.is_none_or(|expected| record.activity == expected)
                && provenance.is_none_or(|expected| record.provenance == expected)
        })
        .collect::<Vec<_>>();
    let coverage = coverage_header(
        store,
        thread_id,
        /*repository_id*/ None,
        filters.time_range()?,
    )
    .await?;
    if json_output {
        println!(
            "{}",
            json!({
                "schemaVersion": 1,
                "kind": "usageActivities",
                "coverage": coverage,
                "data": data.iter().map(|record| json!({
                    "id": record.id.as_string(),
                    "threadId": record.thread_id.as_str(),
                    "agentId": record.agent_id.as_str(),
                    "phase": record.phase.as_str(),
                    "activity": record.activity.as_str(),
                    "state": record.state.as_str(),
                    "startedAtMs": record.started_at_ms,
                    "endedAtMs": record.ended_at_ms,
                    "provenance": record.provenance.as_str(),
                })).collect::<Vec<_>>(),
                "nextCursor": result.next_cursor.as_ref().map(|cursor| encode_cursor(&cursor_kind, cursor)),
                "breakdown": breakdown_activities(&data, &filters.breakdown),
            })
        );
    } else {
        println!("Usage scope: activities");
        println!("Coverage: {coverage}");
        for record in &data {
            println!(
                "{} {} {} {}",
                record.started_at_ms,
                record.phase.as_str(),
                record.activity.as_str(),
                record.provenance.as_str()
            );
        }
        if let Some(cursor) = &result.next_cursor {
            println!("Next cursor: {}", encode_cursor(&cursor_kind, cursor));
        }
    }
    Ok(())
}

async fn list_events(
    store: &UsageStore,
    filters: &UsageFilters,
    page: PageArgs,
    json_output: bool,
) -> Result<(), UsageCommandError> {
    filters.ensure_only(
        &[
            "repository",
            "thread",
            "provenance",
            "coverage",
            "since",
            "until",
        ],
        &["repository", "thread", "provenance", "coverage"],
    )?;
    let repository_id = resolve_optional_repository(store, filters.repository.as_deref()).await?;
    let thread_id = filters.thread_id()?;
    let cursor_kind = filters.cursor_kind("events");
    let result = store
        .list_events(&UsageEventListQuery {
            page: page_request(&page, &cursor_kind)?,
            time_range: filters.time_range()?,
            thread_id: thread_id.clone(),
            repository_id: repository_id.clone(),
            kind: None,
        })
        .await?;
    let provenance = filters.event_provenance()?;
    let coverage = filters.coverage_state()?;
    let data = result
        .data
        .into_iter()
        .filter(|record| {
            provenance.is_none_or(|expected| record.provenance == expected)
                && coverage.is_none_or(|expected| record.coverage == expected)
        })
        .collect::<Vec<_>>();
    let coverage_header =
        coverage_header(store, thread_id, repository_id, filters.time_range()?).await?;
    if json_output {
        println!(
            "{}",
            json!({
                "schemaVersion": 1,
                "kind": "usageEvents",
                "coverage": coverage_header,
                "data": data.iter().map(|record| json!({
                    "id": record.id.as_string(),
                    "threadId": record.thread_id.as_ref().map(ThreadId::as_str),
                    "repositoryId": record.repository_id.as_ref().map(RepositoryId::as_str),
                    "occurredAtMs": record.occurred_at_ms,
                    "event": record.kind.as_str(),
                    "provenance": record.provenance.as_str(),
                    "coverage": record.coverage.as_str(),
                })).collect::<Vec<_>>(),
                "nextCursor": result.next_cursor.as_ref().map(|cursor| encode_cursor(&cursor_kind, cursor)),
                "breakdown": breakdown_events(&data, &filters.breakdown),
            })
        );
    } else {
        println!("Usage scope: events");
        println!("Coverage: {coverage_header}");
        for record in &data {
            println!(
                "{} {} {} {}",
                record.occurred_at_ms,
                record.kind.as_str(),
                record.provenance.as_str(),
                record.coverage.as_str(),
            );
        }
        if let Some(cursor) = &result.next_cursor {
            println!("Next cursor: {}", encode_cursor(&cursor_kind, cursor));
        }
    }
    Ok(())
}

async fn coverage_header(
    store: &UsageStore,
    thread_id: Option<ThreadId>,
    repository_id: Option<RepositoryId>,
    time_range: Option<UtcTimeRange>,
) -> Result<String, UsageCommandError> {
    Ok(store
        .usage_summary_query(UsageSummaryQuery {
            thread_id,
            repository_id,
            account_profile_ref: None,
            time_range,
        })
        .await?
        .coverage
        .overall_state)
}

async fn resolve_repository(
    store: &UsageStore,
    reference: Option<&str>,
) -> Result<RepositoryId, UsageCommandError> {
    if reference.is_none_or(|reference| reference == "current") {
        let current = std::env::current_dir()
            .map_err(|_| UsageCommandError::new(error::UsageErrorKind::NotFound))?;
        return resolve_current_repository(store, &current).await;
    }
    let reference = reference.unwrap_or_default();
    if let Ok(id) = RepositoryId::new(reference) {
        return store
            .read_repository(&id)
            .await?
            .map(|record| record.id)
            .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::NotFound));
    }
    let mut cursor = None;
    let mut scanned = 0_usize;
    let mut match_id = None;
    loop {
        let page = store
            .list_repositories(&UsagePageRequest { cursor, limit: 200 })
            .await?;
        for repository in page.data {
            scanned = scanned
                .checked_add(1)
                .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::Storage))?;
            if scanned > MAX_REPOSITORY_SCAN {
                return Err(UsageCommandError::new(error::UsageErrorKind::Input));
            }
            if repository.label == reference {
                if match_id.is_some() {
                    return Err(UsageCommandError::new(error::UsageErrorKind::Input));
                }
                match_id = Some(repository.id);
            }
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    match_id.ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::NotFound))
}

async fn resolve_current_repository(
    store: &UsageStore,
    current: &std::path::Path,
) -> Result<RepositoryId, UsageCommandError> {
    let workspace =
        codex_git_utils::get_git_repo_root(current).unwrap_or_else(|| current.to_path_buf());
    let workspace = std::fs::canonicalize(workspace)
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::NotFound))?;
    let current_path = workspace
        .to_str()
        .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::Input))?;
    let mut identity = RepositoryIdentityInput::new(
        CanonicalRepositoryPath::new(current_path)
            .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Input))?,
    );
    if let Some(common_dir) = codex_usage::discover_git_common_dir(&workspace) {
        identity = identity.with_git_common_dir(common_dir);
    }
    if let Some(origin) = codex_git_utils::collect_git_info(current)
        .await
        .and_then(|info| info.repository_url)
        .map(RepositoryIdentityMaterial::new)
        .transpose()
        .map_err(|_| UsageCommandError::new(error::UsageErrorKind::Input))?
    {
        identity = identity.with_origin(origin);
    }
    store
        .find_repository_for_identity(&identity)
        .await?
        .ok_or_else(|| UsageCommandError::new(error::UsageErrorKind::NotFound))
}

async fn resolve_optional_repository(
    store: &UsageStore,
    reference: Option<&str>,
) -> Result<Option<RepositoryId>, UsageCommandError> {
    match reference {
        Some(reference) => resolve_repository(store, Some(reference)).await.map(Some),
        None => Ok(None),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn print_mutation(json_output: bool, kind: &str, repository_id: &str) {
    if json_output {
        println!(
            "{}",
            json!({ "schemaVersion": 1, "kind": kind, "repositoryId": repository_id })
        );
    } else {
        println!("Usage repository metadata updated.");
    }
}

#[cfg(test)]
#[path = "usage_cmd_tests.rs"]
mod tests;
