use codex_app_server_protocol::LocalUsageExportFormat;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::Builder;

const MAX_EXPORT_PATH_BYTES: usize = 4_096;

pub(super) struct ExportResult {
    pub file_name: String,
}

pub(super) enum ExportError {
    InvalidDestination,
    Unavailable,
    CommittedDurabilityUncertain,
}

impl ExportError {
    pub fn committed(&self) -> bool {
        matches!(self, Self::CommittedDurabilityUncertain)
    }

    pub fn invalid_destination(&self) -> bool {
        matches!(self, Self::InvalidDestination)
    }
}

impl fmt::Debug for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExportError([redacted])")
    }
}

pub(super) fn create(
    output_path: &str,
    format: LocalUsageExportFormat,
    summary: &LocalUsageSummaryResponse,
) -> Result<ExportResult, ExportError> {
    #[cfg(not(unix))]
    {
        let _ = (output_path, format, summary);
        return Err(ExportError::Unavailable);
    }
    #[cfg(unix)]
    create_unix(output_path, format, summary)
}

#[cfg(unix)]
fn create_unix(
    output_path: &str,
    format: LocalUsageExportFormat,
    summary: &LocalUsageSummaryResponse,
) -> Result<ExportResult, ExportError> {
    use std::os::unix::fs::PermissionsExt;

    if output_path.is_empty()
        || output_path.len() > MAX_EXPORT_PATH_BYTES
        || output_path.chars().any(char::is_control)
    {
        return Err(ExportError::InvalidDestination);
    }
    let requested = PathBuf::from(output_path);
    if !requested.is_absolute() || requested.file_name().is_none() {
        return Err(ExportError::InvalidDestination);
    }
    let parent = requested.parent().ok_or(ExportError::InvalidDestination)?;
    let parent = parent
        .canonicalize()
        .map_err(|_| ExportError::InvalidDestination)?;
    let metadata = parent
        .metadata()
        .map_err(|_| ExportError::InvalidDestination)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(ExportError::InvalidDestination);
    }
    let file_name = requested
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| valid_file_name(name, format))
        .ok_or(ExportError::InvalidDestination)?
        .to_string();
    let target = parent.join(&file_name);
    if target.symlink_metadata().is_ok() {
        return Err(ExportError::InvalidDestination);
    }

    let mut temporary = Builder::new()
        .prefix(".codex-usage-export-")
        .tempfile_in(&parent)
        .map_err(|_| ExportError::Unavailable)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ExportError::Unavailable)?;
    write_export(temporary.as_file_mut(), format, summary)?;
    temporary
        .as_file_mut()
        .flush()
        .map_err(|_| ExportError::Unavailable)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| ExportError::Unavailable)?;
    let persisted = temporary.persist_noclobber(&target).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            ExportError::InvalidDestination
        } else {
            ExportError::Unavailable
        }
    })?;
    persisted
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ExportError::CommittedDurabilityUncertain)?;
    persisted
        .sync_all()
        .map_err(|_| ExportError::CommittedDurabilityUncertain)?;
    sync_directory(&parent).map_err(|_| ExportError::CommittedDurabilityUncertain)?;
    Ok(ExportResult { file_name })
}

#[cfg(unix)]
fn valid_file_name(name: &str, format: LocalUsageExportFormat) -> bool {
    let extension = match format {
        LocalUsageExportFormat::Json => ".json",
        LocalUsageExportFormat::Jsonl => ".jsonl",
        LocalUsageExportFormat::Csv => ".csv",
    };
    !name.is_empty()
        && name.len() <= 255
        && !matches!(name, "." | "..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && name.ends_with(extension)
}

#[cfg(unix)]
fn write_export(
    file: &mut File,
    format: LocalUsageExportFormat,
    summary: &LocalUsageSummaryResponse,
) -> Result<(), ExportError> {
    match format {
        LocalUsageExportFormat::Json => {
            serde_json::to_writer_pretty(&mut *file, summary)
                .map_err(|_| ExportError::Unavailable)?;
            file.write_all(b"\n")
                .map_err(|_| ExportError::Unavailable)?;
        }
        LocalUsageExportFormat::Jsonl => {
            serde_json::to_writer(&mut *file, summary).map_err(|_| ExportError::Unavailable)?;
            file.write_all(b"\n")
                .map_err(|_| ExportError::Unavailable)?;
        }
        LocalUsageExportFormat::Csv => {
            file.write_all(b"categoryKey,count,provenance,coverage\n")
                .map_err(|_| ExportError::Unavailable)?;
            for category in &summary.token_categories {
                let count = category
                    .count
                    .map(|count| count.to_string())
                    .unwrap_or_default();
                let provenance = serde_json::to_value(category.provenance)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .ok_or(ExportError::Unavailable)?;
                let coverage = serde_json::to_value(category.coverage)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .ok_or(ExportError::Unavailable)?;
                writeln!(
                    file,
                    "{},{},{},{}",
                    category.category_key, count, provenance, coverage
                )
                .map_err(|_| ExportError::Unavailable)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
