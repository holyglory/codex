//! Bound each exact index probe to the ids whose Bloom bits match that file.
use std::collections::BTreeMap;
use std::path::Path;

use duckdb::Connection;

use super::OperationAddress;
use super::ReviewPartition;
use super::bloom_maybe_contains;
use crate::Result;
use crate::id_index::id_hash;
use crate::review_state::REVIEW_PARTITION_SIZE;
use crate::review_state::invalid;
use crate::review_storage::parquet_source;
use crate::review_storage::query_rows;
use crate::review_storage::quoted;

const PARTITIONS_PER_QUERY: usize = 32;

pub(super) fn find_addresses(
    root: &Path,
    partitions: &BTreeMap<u64, ReviewPartition>,
    ids: Vec<String>,
    connection: &Connection,
) -> Result<BTreeMap<String, u64>> {
    #[cfg(test)]
    let mut profile = crate::performance_tests::RecoveryProfile::new("operation-addresses");
    if ids.is_empty() || partitions.is_empty() {
        return Ok(BTreeMap::new());
    }
    // Hash and quote a requested identity once. Each file gets only its own
    // positive ids; another file's false positive cannot broaden that predicate.
    let probes = ids
        .iter()
        .map(|id| (id_hash(id), quoted(id)))
        .collect::<Vec<_>>();
    let all_ids = probes
        .iter()
        .map(|(_, id)| id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let partition_refs = partitions.values().collect::<Vec<_>>();
    let mut found = BTreeMap::new();
    #[cfg(test)]
    let mut selected = (0usize, 0usize, 0usize);
    for group in partition_refs.chunks(PARTITIONS_PER_QUERY) {
        let mut queries = Vec::new();
        let mut legacy_files = Vec::new();
        for partition in group {
            if partition.operation_id_bloom.is_empty() {
                // Legacy metadata remains conservative, sharing one all-id
                // predicate rather than copying it into every file's branch.
                legacy_files.push(partition.operation_index_file.clone());
                #[cfg(test)]
                {
                    selected.0 += 1;
                    selected.1 += ids.len();
                }
                continue;
            }
            let candidates = probes
                .iter()
                .filter(|(hash, _)| bloom_maybe_contains(&partition.operation_id_bloom, *hash))
                .map(|(_, id)| id.as_str())
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                continue;
            }
            #[cfg(test)]
            {
                selected.0 += 1;
                selected.1 += candidates.len();
            }
            let source =
                parquet_source(root, std::slice::from_ref(&partition.operation_index_file))?;
            queries.push(format!(
                "SELECT operation_id, first_sequence FROM {source} WHERE operation_id IN ({})",
                candidates.join(", "),
            ));
        }
        if !legacy_files.is_empty() {
            let source = parquet_source(root, &legacy_files)?;
            queries.push(format!(
                "SELECT operation_id, first_sequence FROM {source} WHERE operation_id IN ({all_ids})"
            ));
        }
        #[cfg(test)]
        profile.mark("bloom-partition-routing");
        if queries.is_empty() {
            continue;
        }
        let rows =
            query_rows::<OperationAddress>(connection, &queries.join(" UNION ALL "), ids.len())?;
        #[cfg(test)]
        {
            profile.mark("exact-index-query");
            selected.2 += 1;
        }
        for row in rows {
            if row.first_sequence == 0
                || !partitions.contains_key(&((row.first_sequence - 1) / REVIEW_PARTITION_SIZE))
                || found.insert(row.operation_id, row.first_sequence).is_some()
            {
                return Err(invalid("conflicting current operation addresses"));
            }
        }
    }
    #[cfg(test)]
    if std::env::var_os("CODEX_USAGE_SCALE_PROFILE_RECOVERY").is_some() {
        eprintln!(
            "usage-recovery-profile {}",
            serde_json::json!({
                "component":"operation-addresses","stage":"selection", "requestedIds":ids.len(),
                "candidatePartitions":selected.0,"candidatePairs":selected.1,
                "totalPartitions":partitions.len(),"exactQueries":selected.2,
            })
        );
    }
    Ok(found)
}
