//! Persistent canonical-file reconciliation. Only additions/replacements are
//! decoded; immutable foreground publications never enumerate capture fragments.
use std::fs::File;
use std::fs::TryLockError;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use duckdb::Connection;

use crate::JournalCheckpoint;
use crate::Result;
use crate::UsageHeadlineSnapshot;
use crate::UsageManifest;
use crate::headline::Descriptor;
use crate::headline::METRICS;
use crate::reader_query::milliseconds_expression;
use crate::reclamation::SnapshotPin;
use crate::review_storage::query_error;
use crate::review_storage::quoted;
use crate::review_storage::segment_path;
use crate::storage_file;

const FILE_BATCH: usize = 64;
const DIMENSIONS: &str = "account_id, thread_id, repository_id, model";
const COMPONENTS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
];

struct SpillDirectory(PathBuf);
impl Drop for SpillDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One resumable background reconciliation. Each step imports at most 64 source
/// files. Committed batches survive restart; only complete candidates publish.
pub struct UsageHeadlinePublisher {
    // Fields drop in declaration order: close DuckDB before removing its spill files.
    connection: Connection,
    _spill: SpillDirectory,
    _writer: File,
    _pin: SnapshotPin,
    root: PathBuf,
    manifest: Arc<UsageManifest>,
    source_manifest_identity: crate::metadata_cache::Identity,
    imported_files: usize,
}

impl UsageHeadlinePublisher {
    /// None means another process owns the writer or this prefix is published.
    pub fn open_at(root: impl Into<PathBuf>) -> Result<Option<Self>> {
        let root = root.into();
        storage_file::ensure_usage_directory(&root)?;
        let writer = storage_file::open_read_write(&root.join(".headline.lock"))?;
        match writer.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        drop(storage_file::open_read_write(
            &root.join("headline-segments").join(".private"),
        )?);
        reclaim_outputs(&root)?;
        let pin = SnapshotPin::acquire(&root)?;
        let manifest_path = root.join("manifest.json");
        let source = match File::open(&manifest_path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let source_manifest_identity =
            crate::metadata_cache::Identity::from_metadata(&source.metadata()?);
        if crate::headline::descriptor(&root)?
            .is_some_and(|d| d.source_manifest_identity == format!("{source_manifest_identity:?}"))
        {
            return Ok(None);
        }
        // The opened inode fixes source identity even if publication atomically
        // replaces manifest.json during decoding. Its graph is protected by pin.
        let manifest: Arc<UsageManifest> =
            Arc::new(serde_json::from_reader(std::io::BufReader::new(source))?);
        if manifest.format_version != crate::MANIFEST_FORMAT_VERSION {
            return Err(crate::StorageError::InvalidData(format!(
                "unsupported headline source manifest version {}",
                manifest.format_version
            )));
        }
        if manifest.snapshot_generation == 0 {
            return Ok(None);
        }
        // The writer lock proves no live build owns these crash leftovers.
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with("headline-spill-"))
                && entry.file_type()?.is_dir()
            {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        let spill = SpillDirectory(root.join(format!("headline-spill-{}", stamp())));
        drop(storage_file::open_read_write(&spill.0.join(".private"))?);
        let config = duckdb::Config::default()
            .enable_autoload_extension(/*enabled*/ false)
            .map_err(query_error)?;
        let path = root.join("headline-writer.duckdb");
        let connection = Connection::open_with_flags(&path, config).map_err(query_error)?;
        storage_file::protect_existing(&path)?;
        connection.execute_batch(&format!(
            "SET threads=1; SET memory_limit='128MB'; SET preserve_insertion_order=false; SET temp_directory={}; \
             SET max_temp_directory_size='256MB'; SET wal_autocheckpoint='16MB'; \
             CREATE TABLE IF NOT EXISTS files(file VARCHAR PRIMARY KEY, kind VARCHAR); \
             CREATE TABLE IF NOT EXISTS observations(source_file VARCHAR NOT NULL, id VARCHAR, at_ms BIGINT, \
                account_id VARCHAR, thread_id VARCHAR, repository_id VARCHAR, model VARCHAR, event VARCHAR, \
                input_tokens HUGEINT, cached_input_tokens HUGEINT, output_tokens HUGEINT, reasoning_output_tokens HUGEINT, \
                provider_total_tokens UBIGINT, cache_write_input_tokens UBIGINT, missing_observations UBIGINT); \
             CREATE TABLE IF NOT EXISTS corrections(source_file VARCHAR NOT NULL, id VARCHAR, record_id VARCHAR, \
                input_tokens_delta BIGINT, cached_input_tokens_delta BIGINT, output_tokens_delta BIGINT, reasoning_output_tokens_delta BIGINT); \
             CREATE TABLE IF NOT EXISTS rollups(source_file VARCHAR NOT NULL, first_at_ms BIGINT, last_at_ms BIGINT, \
                account_id VARCHAR, thread_id VARCHAR, repository_id VARCHAR, model VARCHAR, \
                input_tokens HUGEINT, cached_input_tokens HUGEINT, output_tokens HUGEINT, reasoning_output_tokens HUGEINT, \
                token_observations UBIGINT, known_provider_total_tokens HUGEINT, provider_total_unknowns UBIGINT, \
                known_cache_write_input_tokens HUGEINT, cache_write_unknowns UBIGINT, missing_observations UBIGINT); \
             CREATE TEMP TABLE wanted(file VARCHAR PRIMARY KEY, kind VARCHAR, path VARCHAR); \
             CREATE TEMP TABLE applied(id VARCHAR PRIMARY KEY);",
            quoted(&spill.0.to_string_lossy())
        )).map_err(query_error)?;
        {
            let mut appender = connection
                .appender_to_catalog_and_db("wanted", "temp", "main")
                .map_err(query_error)?;
            for (kind, files) in [
                ("observations", &manifest.segment_files),
                ("corrections", &manifest.correction_files),
                ("rollups", &manifest.token_rollup_files),
            ] {
                for file in files {
                    let path = segment_path(&root, file)?;
                    appender
                        .append_row([file.as_str(), kind, path.to_string_lossy().as_ref()])
                        .map_err(query_error)?;
                }
            }
            appender.flush().map_err(query_error)?;
        }
        {
            let mut appender = connection
                .appender_to_catalog_and_db("applied", "temp", "main")
                .map_err(query_error)?;
            for id in &manifest.applied_correction_ids {
                appender.append_row([id.as_str()]).map_err(query_error)?;
            }
            appender.flush().map_err(query_error)?;
        }
        connection.execute_batch(
            "BEGIN; DELETE FROM observations WHERE source_file NOT IN (SELECT file FROM wanted); \
             DELETE FROM corrections WHERE source_file NOT IN (SELECT file FROM wanted); \
             DELETE FROM rollups WHERE source_file NOT IN (SELECT file FROM wanted); \
             DELETE FROM files WHERE file NOT IN (SELECT file FROM wanted); COMMIT; \
             CREATE TEMP TABLE pending AS SELECT w.* FROM wanted w ANTI JOIN files f USING(file);"
        ).map_err(query_error)?;
        Ok(Some(Self {
            connection,
            _spill: spill,
            _writer: writer,
            _pin: pin,
            root,
            manifest,
            source_manifest_identity,
            imported_files: 0,
        }))
    }

    /// Number of source files decoded by this candidate, excluding reused state.
    pub fn imported_files(&self) -> usize {
        self.imported_files
    }

    /// Returns true only after the complete immutable candidate is installed.
    pub fn step(&mut self) -> Result<bool> {
        let connection = &self.connection;
        let mut statement = connection
            .prepare(&format!(
                "SELECT file,kind,path FROM pending LIMIT {FILE_BATCH}"
            ))
            .map_err(query_error)?;
        let files = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(query_error)?
            .collect::<duckdb::Result<Vec<_>>>()
            .map_err(query_error)?;
        drop(statement);
        if files.is_empty() {
            self.publish()?;
            return Ok(true);
        }
        connection.execute_batch("BEGIN").map_err(query_error)?;
        let result = (|| -> Result<()> {
            for (kind, columns) in [
                (
                    "observations",
                    format!(
                        "w.file,id,{} AS at_ms,{DIMENSIONS},event,{},provider_total_tokens,cache_write_input_tokens,missing_observations",
                        milliseconds_expression("observed_at"),
                        COMPONENTS.join(",")
                    ),
                ),
                (
                    "corrections",
                    format!(
                        "w.file,id,record_id,{}",
                        COMPONENTS
                            .iter()
                            .map(|name| format!("{name}_delta"))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                ),
                (
                    "rollups",
                    format!(
                        "w.file,first_at_ms,last_at_ms,{DIMENSIONS},{},token_observations,COALESCE(provider_total_tokens,0),provider_total_unknowns,COALESCE(cache_write_input_tokens,0),cache_write_unknowns,COALESCE(missing_observations,0)",
                        COMPONENTS.join(",")
                    ),
                ),
            ] {
                let paths = files
                    .iter()
                    .filter(|(_, k, _)| k == kind)
                    .map(|(_, _, path)| quoted(path))
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    continue;
                }
                let source = format!(
                    "read_parquet([{}], filename=true, union_by_name=true)",
                    paths.join(",")
                );
                connection.execute_batch(&format!("INSERT INTO {kind} SELECT {columns} FROM {source} p LEFT JOIN wanted w ON p.filename=w.path")).map_err(query_error)?;
            }
            let mut appender = connection.appender("files").map_err(query_error)?;
            for (file, kind, _) in &files {
                appender
                    .append_row([file.as_str(), kind.as_str()])
                    .map_err(query_error)?;
            }
            appender.flush().map_err(query_error)?;
            drop(appender);
            let imported = files
                .iter()
                .map(|(file, _, _)| quoted(file))
                .collect::<Vec<_>>()
                .join(",");
            connection
                .execute_batch(&format!("DELETE FROM pending WHERE file IN ({imported})"))
                .map_err(query_error)?;
            connection.execute_batch("COMMIT").map_err(query_error)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = connection.execute_batch("ROLLBACK");
        }
        result?;
        self.imported_files += files.len();
        Ok(false)
    }

    fn publish(&self) -> Result<()> {
        let connection = &self.connection;
        // Import routing can contain hundreds of thousands of paths and an ART
        // index. Release it before sorting the much smaller token publication.
        connection
            .execute_batch("DROP TABLE wanted; DROP TABLE pending")
            .map_err(query_error)?;
        // Long-lived reader pins may defer global reclamation. Keep publication
        // fanout bounded while capture and private incremental reconciliation continue.
        let mut outputs = 0;
        for entry in std::fs::read_dir(self.root.join("headline-segments"))? {
            if entry?
                .path()
                .extension()
                .is_some_and(|extension| extension == "parquet")
            {
                outputs += 1;
            }
        }
        if outputs >= 16 {
            return Err(crate::StorageError::InvalidData(
                "headline publication awaits pinned-file reclamation".into(),
            ));
        }
        let deltas = COMPONENTS
            .iter()
            .map(|name| format!("sum({name}_delta::HUGEINT) {name}_delta"))
            .collect::<Vec<_>>()
            .join(",");
        let corrected = COMPONENTS
            .iter()
            .map(|name| format!("o.{name}+COALESCE(c.{name}_delta,0) AS {name}"))
            .collect::<Vec<_>>()
            .join(",");
        let token = "event='token_observation' OR provider_total_tokens IS NOT NULL OR input_tokens<>0 OR cached_input_tokens<>0 OR output_tokens<>0 OR reasoning_output_tokens<>0";
        // DuckDB writes HUGEINT as Parquet DOUBLE. Keep checked integer output
        // types so large provider-reported values never lose low-order bits.
        let metrics = METRICS
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let kind = if index < 4 { "BIGINT" } else { "UBIGINT" };
                format!("{name}::{kind} AS {name}")
            })
            .collect::<Vec<_>>()
            .join(",");
        let meaningful = METRICS
            .iter()
            .map(|name| format!("{name}<>0"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let query = format!(
            "WITH corrected AS (SELECT o.* EXCLUDE(input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens),{corrected} \
             FROM observations o LEFT JOIN (SELECT record_id,{deltas} FROM corrections JOIN applied USING(id) GROUP BY record_id) c ON o.id=c.record_id), \
             tokens AS (SELECT *,({token}) AS is_token FROM corrected), \
             exact AS (SELECT at_ms first_at_ms,at_ms last_at_ms,{DIMENSIONS}, \
                input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens, \
                (is_token IS TRUE)::UBIGINT token_observations,COALESCE(provider_total_tokens,0) known_provider_total_tokens, \
                (is_token IS TRUE AND provider_total_tokens IS NULL)::UBIGINT provider_total_unknowns, \
                COALESCE(cache_write_input_tokens,0) known_cache_write_input_tokens, \
                (is_token IS TRUE AND cache_write_input_tokens IS NULL)::UBIGINT cache_write_unknowns, \
                COALESCE(missing_observations,0) missing_observations \
                FROM tokens WHERE is_token OR cache_write_input_tokens IS NOT NULL OR missing_observations>0) \
             SELECT first_at_ms,last_at_ms,{DIMENSIONS},{metrics} FROM \
                (SELECT * FROM exact UNION ALL SELECT * EXCLUDE(source_file) FROM rollups WHERE {meaningful}) \
             ORDER BY repository_id,first_at_ms"
        );
        let id = stamp();
        let file = format!("headline-{id}.parquet");
        let target = crate::headline::data_path(&self.root, &file)?;
        connection
            .execute_batch(&format!(
                "COPY ({query}) TO {} (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 2048)",
                quoted(&target.to_string_lossy())
            ))
            .map_err(query_error)?;
        storage_file::finish_generated(&target)?;
        // Reclaim deleted pages for reuse and bound the private writer's WAL
        // before advancing the last successfully published source identity.
        connection
            .execute_batch("CHECKPOINT")
            .map_err(query_error)?;
        let manifest = &self.manifest;
        let snapshot = UsageHeadlineSnapshot {
            snapshot_id: id.clone(),
            source_snapshot_generation: manifest.snapshot_generation,
            source_watermark: JournalCheckpoint {
                sequence: manifest.watermark,
                byte_offset: manifest.journal_byte_offset,
            },
            source_watermark_at_ms: manifest.watermark_at_ms,
            source_freshness: manifest.freshness,
            published_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64,
            pending_corrections: manifest.pending_corrections.len() as u64,
            retired_observations: manifest.retired_observations,
            raw_admission_cutoff_at_ms: manifest.raw_admission_cutoff_at_ms,
            unapplied_retention_records: manifest.unapplied_retention_records,
            unapplied_retention_corrections: manifest.unapplied_retention_corrections,
            latest_journal_watermark: manifest.watermark,
            pending_entries: 0,
            freshness: manifest.freshness,
        };
        let temporary = self.root.join(format!("headline-{id}.tmp"));
        let mut output = storage_file::create_new(&temporary)?;
        output.write_all(&serde_json::to_vec(&Descriptor {
            version: 1,
            data_file: file,
            source_manifest_identity: format!("{:?}", self.source_manifest_identity),
            snapshot,
        })?)?;
        output.sync_all()?;
        drop(output);
        #[cfg(unix)]
        File::open(self.root.join("headline-segments"))?.sync_all()?;
        std::fs::rename(&temporary, self.root.join("headline.json"))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

/// Called while holding the headline writer lock. Admission closes only while
/// old file pins are checked; long-lived views never block unrelated outputs.
fn reclaim_outputs(root: &Path) -> Result<()> {
    let admission = storage_file::open_read_write(&root.join(".headline-readers.lock"))?;
    match admission.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let current = crate::headline::descriptor(root)?.map(|descriptor| descriptor.data_file);
    for entry in std::fs::read_dir(root.join("headline-segments"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".parquet") || current.as_deref() == Some(name) {
            continue;
        }
        let path = crate::headline::data_path(root, name)?;
        let pin_path = path.with_extension("pin");
        let pin = storage_file::open_read_write(&pin_path)?;
        match pin.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        std::fs::remove_file(path)?;
        drop(pin);
        std::fs::remove_file(pin_path)?;
    }
    Ok(())
}

fn stamp() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub(crate) fn run(root: PathBuf, receiver: mpsc::Receiver<()>) {
    let mut pending = false;
    loop {
        if !pending
            && matches!(
                receiver.recv_timeout(Duration::from_secs(30)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            )
        {
            return;
        }
        pending = false;
        // A fixed coalescing window bounds exports during continuous capture.
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while let Some(wait) = until.checked_duration_since(std::time::Instant::now()) {
            if matches!(
                receiver.recv_timeout(wait),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ) {
                return;
            }
        }
        let result = (|| -> Result<()> {
            let Some(mut build) = UsageHeadlinePublisher::open_at(&root)? else {
                return Ok(());
            };
            loop {
                match receiver.try_recv() {
                    Ok(()) => pending = true,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                if build.step()? {
                    return Ok(());
                }
            }
        })();
        if result.is_err() {
            tracing::debug!("usage token headline publication deferred after failure");
        }
    }
}
