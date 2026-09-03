use super::*;

#[derive(Clone)]
struct OperationLinks {
    parent_operation_id: Option<String>,
    retry_of_operation_id: Option<String>,
    is_explicit_rework: bool,
}

impl UsageStore {
    pub(super) async fn task_tree_rework_flags(
        &self,
        operations: &[OperationRow],
    ) -> Result<HashMap<String, bool>, UsageStoreError> {
        let mut links = operations
            .iter()
            .map(|operation| {
                (
                    operation.id.clone(),
                    OperationLinks {
                        parent_operation_id: operation.parent_operation_id.clone(),
                        retry_of_operation_id: operation.retry_of_operation_id.clone(),
                        is_explicit_rework: operation.rework_of_operation_id.is_some(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let mut pending = referenced_operations(links.values(), &links);
        let mut ancestor_count = 0_usize;
        while !pending.is_empty() {
            let batch = pending.iter().take(400).cloned().collect::<Vec<_>>();
            for operation_id in &batch {
                pending.remove(operation_id);
            }
            let mut query = QueryBuilder::<Sqlite>::new(
                "SELECT id, parent_operation_id, retry_of_operation_id, rework_of_operation_id \
                 FROM operations WHERE id IN (",
            );
            push_string_binds(&mut query, &batch);
            query.push(')');
            let rows = query
                .build()
                .fetch_all(&self.pool)
                .await
                .map_err(UsageStoreError::Database)?;
            if rows.len() != batch.len() {
                return Err(UsageStoreError::InvalidFact);
            }
            ancestor_count = ancestor_count
                .checked_add(rows.len())
                .ok_or(UsageStoreError::AggregateOverflow)?;
            if ancestor_count > MAX_OPERATION_LINK_VISITS {
                return Err(UsageStoreError::TaskTreeTooLarge);
            }
            for row in rows {
                let operation_id: String = row.get("id");
                links.insert(
                    operation_id,
                    OperationLinks {
                        parent_operation_id: row.get("parent_operation_id"),
                        retry_of_operation_id: row.get("retry_of_operation_id"),
                        is_explicit_rework: row
                            .get::<Option<String>, _>("rework_of_operation_id")
                            .is_some(),
                    },
                );
            }
            pending.extend(referenced_operations(links.values(), &links));
        }

        let mut cache = HashMap::new();
        for operation in operations {
            let is_rework = operation_is_rework(&operation.id, &links, &cache)?;
            cache.insert(operation.id.clone(), is_rework);
        }
        Ok(cache)
    }
}

fn referenced_operations<'a>(
    operations: impl Iterator<Item = &'a OperationLinks>,
    known: &HashMap<String, OperationLinks>,
) -> HashSet<String> {
    operations
        .flat_map(|operation| {
            [
                operation.parent_operation_id.as_ref(),
                operation.retry_of_operation_id.as_ref(),
            ]
        })
        .flatten()
        .filter(|operation_id| !known.contains_key(operation_id.as_str()))
        .cloned()
        .collect()
}

fn operation_is_rework(
    start: &str,
    operations: &HashMap<String, OperationLinks>,
    cache: &HashMap<String, bool>,
) -> Result<bool, UsageStoreError> {
    let mut pending = vec![start];
    let mut visited = HashSet::new();
    while let Some(id) = pending.pop() {
        if cache.get(id).copied().unwrap_or(false) {
            return Ok(true);
        }
        if !visited.insert(id) {
            continue;
        }
        if visited.len() > MAX_OPERATION_LINK_VISITS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        let operation = operations.get(id).ok_or(UsageStoreError::InvalidFact)?;
        if operation.is_explicit_rework {
            return Ok(true);
        }
        if let Some(parent) = operation.parent_operation_id.as_deref() {
            pending.push(parent);
        }
        if let Some(retry) = operation.retry_of_operation_id.as_deref() {
            pending.push(retry);
        }
    }
    Ok(false)
}
