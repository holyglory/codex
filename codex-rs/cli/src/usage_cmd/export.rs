use super::ExportFormat;
use super::error::UsageCommandError;
use super::error::UsageErrorKind;
use codex_usage::CoverageState;
use codex_usage::UsageEventListQuery;
use codex_usage::UsageEventProvenance;
use codex_usage::UsagePageRequest;
use codex_usage::UsageStore;
use serde_json::json;
use std::io::Write;
use std::path::Path;

const EXPORT_PAGE_SIZE: u32 = 500;

pub(crate) async fn write(
    store: &UsageStore,
    output: &Path,
    format: ExportFormat,
    mut query: UsageEventListQuery,
    provenance: Option<UsageEventProvenance>,
    coverage: Option<CoverageState>,
) -> Result<u64, UsageCommandError> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() || output.exists() {
        return Err(UsageCommandError::new(UsageErrorKind::Export));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
    }
    if format == ExportFormat::Csv {
        temporary
            .write_all(b"schema_version,event_id,thread_id,repository_id,occurred_at_ms,kind,provenance,coverage\n")
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
    }
    let mut count = 0_u64;
    query.page = UsagePageRequest {
        cursor: None,
        limit: EXPORT_PAGE_SIZE,
    };
    loop {
        let page = store.list_events(&query).await?;
        for event in page.data {
            if provenance.is_some_and(|expected| event.provenance != expected)
                || coverage.is_some_and(|expected| event.coverage != expected)
            {
                continue;
            }
            match format {
                ExportFormat::Jsonl => {
                    let line = serde_json::to_vec(&json!({
                        "schemaVersion": 1,
                        "eventId": event.id.as_string(),
                        "threadId": event.thread_id.as_ref().map(codex_usage::ThreadId::as_str),
                        "repositoryId": event.repository_id.as_ref().map(codex_usage::RepositoryId::as_str),
                        "occurredAtMs": event.occurred_at_ms,
                        "kind": event.kind.as_str(),
                        "provenance": event.provenance.as_str(),
                        "coverage": event.coverage.as_str(),
                    }))
                    .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
                    temporary
                        .write_all(&line)
                        .and_then(|()| temporary.write_all(b"\n"))
                        .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
                }
                ExportFormat::Csv => {
                    let line = [
                        "1".to_string(),
                        event.id.as_string(),
                        event
                            .thread_id
                            .as_ref()
                            .map_or_else(String::new, |id| id.as_str().to_string()),
                        event
                            .repository_id
                            .as_ref()
                            .map_or_else(String::new, |id| id.as_str().to_string()),
                        event.occurred_at_ms.to_string(),
                        event.kind.as_str().to_string(),
                        event.provenance.as_str().to_string(),
                        event.coverage.as_str().to_string(),
                    ]
                    .into_iter()
                    .map(|field| csv_field(&field))
                    .collect::<Vec<_>>()
                    .join(",");
                    writeln!(temporary, "{line}")
                        .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
                }
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| UsageCommandError::new(UsageErrorKind::Export))?;
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        query.page.cursor = Some(cursor);
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
    temporary
        .persist_noclobber(output)
        .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
    sync_parent(parent)?;
    Ok(count)
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn sync_parent(parent: &Path) -> Result<(), UsageCommandError> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Export))?;
    }
    Ok(())
}
