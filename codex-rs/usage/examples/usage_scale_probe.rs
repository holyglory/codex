//! Explicit real-store diagnostics. Snapshot mode copies only published usage
//! facts, without credentials, rollouts, a mutable tail, or producer mutations.
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use codex_usage::UsageReader;
use serde_json::Value;
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [mode, root, rest @ ..] = arguments.as_slice() else {
        return Err("expected snapshot ROOT DESTINATION or profile ROOT SCOPE_FILE FROM TO".into());
    };
    let started = Instant::now();
    let reader = UsageReader::open_at(root)?;
    println!(
        "{}",
        json!({"stage":"reader_open", "elapsedMs":started.elapsed().as_secs_f64()*1000.0,
        "generation":reader.manifest().snapshot_generation,"watermark":reader.manifest().watermark,
        "rawFiles":reader.manifest().segment_files.len()})
    );
    match (mode.as_str(), rest) {
        ("snapshot", [destination]) => snapshot(Path::new(root), Path::new(destination), &reader),
        ("profile", [scope, from, to]) => {
            let scope: Value = serde_json::from_slice(&fs::read(scope)?)?;
            let filter = UsageFilter {
                repository_id: Some(
                    scope["repositoryId"]
                        .as_str()
                        .ok_or("scope has no repository ID")?
                        .into(),
                ),
                ..Default::default()
            };
            let range = TimeRange::from_millis(from.parse()?, to.parse()?)?;
            let prepared = Instant::now();
            let window = reader.prepare_window(range, &filter)?;
            println!(
                "{}",
                json!({"stage":"prepare_window", "elapsedMs":prepared.elapsed().as_secs_f64()*1000.0})
            );
            let summary = Instant::now();
            let totals = window.summary()?;
            println!(
                "{}",
                json!({"stage":"summary", "elapsedMs":summary.elapsed().as_secs_f64()*1000.0,"value":totals})
            );
            let review = Instant::now();
            let facts = window.performance_review()?;
            println!(
                "{}",
                json!({"stage":"review", "elapsedMs":review.elapsed().as_secs_f64()*1000.0,"value":facts})
            );
            Ok(())
        }
        _ => Err("invalid probe arguments".into()),
    }
}

fn snapshot(root: &Path, destination: &Path, reader: &UsageReader) -> Result<()> {
    // Holding this ordinary reader pins every immutable input. Reclamation
    // skips a pinned store; publication and journal appends remain independent.
    fs::create_dir(destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    }
    fs::create_dir(destination.join("segments"))?;
    let started = Instant::now();
    let manifest = serde_json::to_value(reader.manifest())?;
    let mut files = BTreeSet::new();
    referenced_files(&manifest, &mut files);
    let mut pending = files
        .iter()
        .filter(|file| file.ends_with(".json"))
        .cloned()
        .collect::<Vec<_>>();
    let mut parsed = BTreeSet::new();
    while let Some(file) = pending.pop() {
        if !parsed.insert(file.clone()) {
            continue;
        }
        let descriptor: Value =
            serde_json::from_slice(&fs::read(root.join("segments").join(file))?)?;
        let mut references = BTreeSet::new();
        referenced_files(&descriptor, &mut references);
        for reference in references {
            if files.insert(reference.clone()) && reference.ends_with(".json") {
                pending.push(reference);
            }
        }
        if files.len() > 2_000_000 {
            return Err("diagnostic snapshot exceeds two million referenced files".into());
        }
    }
    // Copy through the manifest's logical offset. The fixed journal header
    // accounts for a retired physical prefix; no appending tail is included.
    let mut source = fs::File::open(root.join("events.jsonl"))?;
    let mut header = [0u8; 96];
    let read = source.read(&mut header)?;
    let physical_end = if header[..read].starts_with(b"{\"journalBase\":[") {
        let value: Value = serde_json::from_slice(&header)?;
        reader
            .manifest()
            .journal_byte_offset
            .checked_sub(
                value["journalBase"][1]
                    .as_u64()
                    .ok_or("invalid journal base")?,
            )
            .and_then(|value| value.checked_add(96))
            .ok_or("journal prefix was retired")?
    } else {
        reader.manifest().journal_byte_offset
    };
    let source = fs::File::open(root.join("events.jsonl"))?;
    let mut output = fs::File::create(destination.join("events.jsonl"))?;
    if std::io::copy(&mut source.take(physical_end), &mut output)? != physical_end {
        return Err("journal snapshot was truncated".into());
    }
    let total = files.len();
    let bytes = AtomicU64::new(physical_end);
    let copied = AtomicUsize::new(0);
    let files = files.into_iter().collect::<Vec<_>>();
    std::thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::new();
        for chunk in files.chunks(files.len().div_ceil(8).max(1)) {
            let bytes = &bytes;
            let copied = &copied;
            workers.push(scope.spawn(move || -> Result<()> {
                for file in chunk {
                    let length = fs::copy(root.join("segments").join(file), destination.join("segments").join(file))?;
                    bytes.fetch_add(length, Ordering::Relaxed);
                    let count = copied.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 10_000 == 0 {
                        println!("{}", json!({"stage":"snapshot_copy","copiedFiles":count,"totalFiles":total,
                            "bytes":bytes.load(Ordering::Relaxed),"elapsedMs":started.elapsed().as_secs_f64()*1000.0}));
                    }
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker
                .join()
                .map_err(|_| "snapshot copy worker panicked")??;
        }
        Ok(())
    })?;
    let bytes = bytes.load(Ordering::Relaxed);
    fs::write(
        destination.join("manifest.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    let copied = UsageReader::open_at(destination)?;
    if copied.manifest() != reader.manifest()
        || copied.journal_watermark() != reader.manifest().watermark
    {
        return Err("copied snapshot does not match pinned publication".into());
    }
    let proof = json!({"stage":"snapshot_complete","files":total,"bytes":bytes,
        "elapsedMs":started.elapsed().as_secs_f64()*1000.0,
        "generation":reader.manifest().snapshot_generation,"watermark":reader.manifest().watermark,
        "scope":"published usage facts only; mutable tail and work-binding side store excluded"});
    fs::write(
        destination.join("probe-snapshot.json"),
        serde_json::to_vec_pretty(&proof)?,
    )?;
    println!("{proof}");
    Ok(())
}

fn referenced_files(value: &Value, files: &mut BTreeSet<String>) {
    match value {
        Value::String(value)
            if [
                "segment-",
                "correction-",
                "membership-",
                "lineage-",
                "record-id-",
                "review-",
                "rollup-",
            ]
            .iter()
            .any(|prefix| value.starts_with(prefix))
                && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
                && [".parquet", ".json", ".bin"]
                    .iter()
                    .any(|suffix| value.ends_with(suffix)) =>
        {
            files.insert(value.clone());
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| referenced_files(value, files)),
        Value::Object(values) => values
            .values()
            .for_each(|value| referenced_files(value, files)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}
