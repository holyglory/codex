use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;

use duckdb::Connection;

use crate::Result;
use crate::StorageError;
use crate::UsageManifest;
use crate::UsageRecord;
use crate::storage_file;

/// Whether a task-tree query includes explicitly linked child threads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadLineageScope {
    /// Return only the requested root, without opening lineage storage.
    RootOnly,
    /// Include all transitively linked children, validating the complete graph.
    Descendants,
}

/// Resolve explicit ancestry from one pinned sidecar and its complete hot tail.
/// Results are sorted and always include the requested root. Missing parent
/// facts remain unknown; record names, timing and operation ownership imply none.
pub fn read_descendant_threads(
    root: &Path,
    manifest: &UsageManifest,
    root_thread_id: &str,
    scope: ThreadLineageScope,
    tail: &[UsageRecord],
) -> Result<Vec<String>> {
    if scope == ThreadLineageScope::RootOnly {
        return Ok(vec![root_thread_id.to_string()]);
    }
    let mut graph = load_graph(root, manifest.lineage_file.as_deref())?;
    // Canonical record identity is first-write-wins, including within the tail.
    let mut records = BTreeMap::new();
    for record in tail {
        records.entry(record.id.as_str()).or_insert(record);
    }
    records.retain(|_, record| record.parent_thread_id.is_some());
    let existing = published_ids(root, manifest, records.keys().copied().collect())?;
    for (id, record) in records {
        if existing.contains(id) {
            continue;
        }
        let child = record
            .thread_id
            .as_deref()
            .ok_or_else(|| invalid("parent fact has no child thread id"))?;
        if let Some(parent) = record.parent_thread_id.as_deref() {
            add_edge(&mut graph, child, parent)?;
        }
    }
    validate_graph(&graph)?;
    let mut children = BTreeMap::<&str, Vec<&str>>::new();
    for (child, parent) in &graph {
        children.entry(parent).or_default().push(child);
    }
    let mut selected = BTreeSet::from([root_thread_id.to_string()]);
    let mut queue = VecDeque::from([root_thread_id]);
    while let Some(parent) = queue.pop_front() {
        if let Some(descendants) = children.get(parent) {
            for &child in descendants {
                if selected.insert(child.to_string()) {
                    queue.push_back(child);
                }
            }
        }
    }
    Ok(selected.into_iter().collect())
}

/// Merge only newly accepted facts; old operation segments are never scanned.
pub(crate) fn publish_thread_lineage(
    root: &Path,
    previous: Option<&str>,
    file: &str,
    additions: &[(String, String)],
) -> Result<Option<String>> {
    if additions.is_empty() {
        return Ok(previous.map(str::to_string));
    }
    storage_file::ensure_usage_directory(root)?;
    let mut graph = load_graph(root, previous)?;
    let before = graph.len();
    for (child, parent) in additions {
        add_edge(&mut graph, child, parent)?;
    }
    validate_graph(&graph)?;
    if graph.len() == before {
        return Ok(previous.map(str::to_string));
    }
    let values = graph
        .iter()
        .map(|(child, parent)| format!("({}, {})", quoted(child), quoted(parent)))
        .collect::<Vec<_>>()
        .join(", ");
    let path = lineage_path(root, file)?;
    let query = format!(
        "COPY (SELECT * FROM (VALUES {values}) AS lineage(child_thread_id, parent_thread_id) ORDER BY child_thread_id, parent_thread_id) TO {} (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 2048)",
        quoted(&path.to_string_lossy())
    );
    Connection::open_in_memory()
        .map_err(query_error)?
        .execute_batch(&query)
        .map_err(query_error)?;
    storage_file::finish_generated(&path)?;
    Ok(Some(file.to_string()))
}

fn load_graph(root: &Path, file: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut graph = BTreeMap::new();
    let Some(file) = file else {
        return Ok(graph);
    };
    let path = lineage_path(root, file)?;
    storage_file::protect_existing(&path)?;
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let mut statement = connection.prepare(&format!("SELECT child_thread_id, parent_thread_id FROM read_parquet({}) ORDER BY child_thread_id, parent_thread_id", quoted(&path.to_string_lossy()))).map_err(query_error)?;
    let mut rows = statement.query([]).map_err(query_error)?;
    while let Some(row) = rows.next().map_err(query_error)? {
        let child: String = row.get(0).map_err(query_error)?;
        let parent: String = row.get(1).map_err(query_error)?;
        add_edge(&mut graph, &child, &parent)?;
    }
    Ok(graph)
}

fn add_edge(graph: &mut BTreeMap<String, String>, child: &str, parent: &str) -> Result<()> {
    if child.is_empty() || parent.is_empty() || child == parent {
        return Err(invalid("invalid child/parent thread link"));
    }
    if let Some(previous) = graph.get(child) {
        if previous != parent {
            return Err(invalid(&format!("conflicting parents for thread {child}")));
        }
    } else {
        graph.insert(child.to_string(), parent.to_string());
    }
    Ok(())
}

fn validate_graph(graph: &BTreeMap<String, String>) -> Result<()> {
    let mut resolved = BTreeSet::new();
    for start in graph.keys() {
        let mut path = BTreeSet::new();
        let mut current = start.as_str();
        while let Some(parent) = graph.get(current) {
            if resolved.contains(current) {
                break;
            }
            if !path.insert(current) {
                return Err(invalid(&format!("thread lineage cycle at {current}")));
            }
            current = parent;
        }
        resolved.extend(path);
    }
    Ok(())
}

fn published_ids(
    root: &Path,
    manifest: &UsageManifest,
    candidates: Vec<&str>,
) -> Result<BTreeSet<String>> {
    if candidates.is_empty() || manifest.segment_files.is_empty() {
        return Ok(BTreeSet::new());
    }
    let paths = manifest
        .segment_files
        .iter()
        .map(|file| lineage_path(root, file).map(|path| quoted(&path.to_string_lossy())))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let candidates = candidates
        .into_iter()
        .map(quoted)
        .collect::<Vec<_>>()
        .join(", ");
    let connection = Connection::open_in_memory().map_err(query_error)?;
    let mut statement = connection
        .prepare(&format!(
            "SELECT DISTINCT id FROM read_parquet([{paths}]) WHERE id IN ({candidates})"
        ))
        .map_err(query_error)?;
    let rows = statement
        .query_map([], |row| row.get(0))
        .map_err(query_error)?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(query_error)
}

fn lineage_path(root: &Path, file: &str) -> Result<PathBuf> {
    if Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file) {
        return Err(invalid("lineage segment must be a file name"));
    }
    Ok(root.join("segments").join(file))
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidData(message.into())
}
fn query_error(error: duckdb::Error) -> StorageError {
    StorageError::Columnar(format!("DuckDB lineage query failed: {error}"))
}
