//! Explicit governed preparation of the normal derived headline publication.
//! Uses only the normal public headline publisher and journal checkpoint API.
//! No report reader, token query, service control, seed, migration, or reset.
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use codex_usage::MANIFEST_FORMAT_VERSION;
use codex_usage::StorageError;
use codex_usage::USAGE_DIRECTORY_NAME;
use codex_usage::UsageHeadlinePublisher;
use codex_usage::UsageHeadlineSnapshot;
use codex_usage::UsageJournal;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestHeader {
    format_version: u32,
    snapshot_generation: u64,
    watermark: u64,
}

#[derive(Deserialize)]
struct Published {
    version: u32,
    data_file: String,
    snapshot: UsageHeadlineSnapshot,
}

fn require(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(reason.into())
    }
}

fn storage<T>(result: codex_usage::Result<T>) -> Result<T> {
    result.map_err(|error| -> Box<dyn std::error::Error> {
        // Keep content, SQL, paths, and identity values out of governed progress.
        match error {
            StorageError::Io(_) => "usage_storage_io_failed",
            StorageError::Json(_) => "usage_storage_json_failed",
            StorageError::InvalidData(_) => "usage_storage_invariant_failed",
            StorageError::OperationIndexAtCapacity => "usage_operation_index_at_capacity",
            StorageError::UnsupportedFormat(_) => "usage_storage_format_unavailable",
            StorageError::Columnar(_) => "usage_columnar_publication_failed",
        }
        .into()
    })
}

fn header(root: &Path) -> Result<ManifestHeader> {
    // Ignore the large file graph while streaming the current manifest. The
    // producer separately opens and validates its exact source inode normally.
    let value: ManifestHeader =
        serde_json::from_reader(BufReader::new(File::open(root.join("manifest.json"))?))?;
    require(
        value.format_version == MANIFEST_FORMAT_VERSION,
        "current_usage_format_required",
    )?;
    require(
        value.snapshot_generation != 0,
        "canonical_publication_required",
    )?;
    Ok(value)
}

fn published(root: &Path) -> Result<UsageHeadlineSnapshot> {
    // Metadata admission only: no UsageReader/UsageHeadlineReader, query engine,
    // or tail replay. The pin protects the existence check against headline GC.
    let admission = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(".headline-readers.lock"))?;
    admission.lock_shared()?;
    let mut bytes = Vec::new();
    File::open(root.join("headline.json"))?
        .take(65_537)
        .read_to_end(&mut bytes)?;
    require(bytes.len() <= 65_536, "oversized_headline_descriptor")?;
    let value: Published = serde_json::from_slice(&bytes)?;
    require(
        value.version == 2 && value.snapshot.source_snapshot_generation != 0,
        "current_headline_publication_required",
    )?;
    require(
        Path::new(&value.data_file)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(value.data_file.as_str())
            && value.data_file.starts_with("headline-")
            && value.data_file.ends_with(".parquet"),
        "invalid_headline_filename",
    )?;
    let data = root.join("headline-segments").join(&value.data_file);
    let pin = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.with_extension("pin"))?;
    pin.lock_shared()?;
    require(
        fs::symlink_metadata(data)?.file_type().is_file(),
        "published_headline_file_missing",
    )?;
    Ok(value.snapshot)
}

fn write_receipt(path: &Path, value: &Value) -> Result<()> {
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    output.write_all(&serde_json::to_vec_pretty(value)?)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    #[cfg(unix)]
    File::open(path.parent().ok_or("receipt_parent_required")?)?.sync_all()?;
    Ok(())
}

fn run() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    require(args.len() == 2, "expected_ROOT_OUTPUT_RECEIPT")?;
    let requested = PathBuf::from(&args[0]);
    require(
        requested.is_absolute()
            && requested
                .file_name()
                .is_some_and(|name| name == USAGE_DIRECTORY_NAME),
        "explicit_absolute_usage_root_required",
    )?;
    require(
        fs::symlink_metadata(&requested)?.file_type().is_dir(),
        "regular_usage_directory_required",
    )?;
    let root = requested.canonicalize()?;
    require(root == requested, "canonical_usage_root_required")?;
    let output = PathBuf::from(&args[1]);
    require(
        output.is_absolute() && !output.starts_with(&root) && !output.try_exists()?,
        "new_external_receipt_required",
    )?;
    let output_parent = output
        .parent()
        .ok_or("receipt_parent_required")?
        .canonicalize()?;
    require(
        !output_parent.starts_with(&root),
        "receipt_must_be_outside_usage_root",
    )?;
    let output = output_parent.join(output.file_name().ok_or("receipt_filename_required")?);
    let started = Instant::now();
    let initial = header(&root)?;
    println!(
        "{}",
        json!({"stage":"headline_preparation_started","canonicalGeneration":initial.snapshot_generation,
        "canonicalWatermark":initial.watermark,"maximumPublications":3})
    );
    let mut imported = 0usize;
    let mut next_progress = 4_096usize;
    let mut publications = 0;
    for candidate in 1..=3 {
        let Some(mut publisher) = storage(UsageHeadlinePublisher::open_at(&root))? else {
            break;
        };
        println!(
            "{}",
            json!({"stage":"headline_candidate_started","candidate":candidate,"importedFiles":imported,
            "elapsedMs":started.elapsed().as_millis()})
        );
        loop {
            let complete = storage(publisher.step())?;
            let count = imported
                .checked_add(publisher.imported_files())
                .ok_or("imported_file_count_overflow")?;
            if count >= next_progress {
                println!(
                    "{}",
                    json!({"stage":"headline_import_progress","candidate":candidate,"importedFiles":count,
                    "elapsedMs":started.elapsed().as_millis()})
                );
                next_progress = count
                    .checked_div(4_096)
                    .and_then(|value| value.checked_add(1))
                    .and_then(|value| value.checked_mul(4_096))
                    .ok_or("progress_count_overflow")?;
            }
            if complete {
                imported = count;
                publications += 1;
                break;
            }
        }
        // Drop releases the normal producer lock before an optional catch-up.
        drop(publisher);
    }
    let snapshot = published(&root)?;
    let latest = storage(UsageJournal::new(root.join("events.jsonl")).checkpoint())?;
    let pending = latest
        .sequence
        .checked_sub(snapshot.source_watermark.sequence)
        .ok_or("headline_exceeds_journal")?;
    let result = json!({"stage":"headline_preparation_complete","ready":true,"basis":"normal_derived_publication",
        "sourceGeneration":snapshot.source_snapshot_generation,"watermark":snapshot.source_watermark.sequence,
        "sourceWatermarkAtMs":snapshot.source_watermark_at_ms,"publishedAtMs":snapshot.published_at_ms,
        "sourceFreshness":snapshot.source_freshness,"latestJournalWatermark":latest.sequence,"pendingEntries":pending,
        "pendingCorrections":snapshot.pending_corrections,"publications":publications,"maximumPublications":3,
        "importedFiles":imported,"elapsedMs":started.elapsed().as_millis(),"backlogIncluded":false});
    write_receipt(&output, &result)?;
    println!("{result}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        // Avoid raw I/O/JSON diagnostics carrying paths or fact contents.
        let reason = error
            .downcast_ref::<StorageError>()
            .map_or("headline_preparation_failed", |_| "headline_storage_failed");
        eprintln!(
            "{}",
            json!({"stage":"headline_preparation_failed","reason":reason})
        );
        std::process::exit(1);
    }
}
