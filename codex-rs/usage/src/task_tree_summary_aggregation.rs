use super::*;

#[derive(Clone, Eq, PartialEq)]
struct TokenFact {
    operation_id: String,
    token_count: Option<i64>,
    coverage: String,
}

impl UsageStore {
    pub(super) async fn task_tree_tool_roles(
        &self,
        thread_ids: &[String],
        range: UtcTimeRange,
    ) -> Result<(HashSet<String>, ToolCounts), UsageStoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            r#"SELECT operation.id, tool.execution_group_id, tool.execution_role
               FROM operations AS operation
               JOIN tool_invocations AS tool ON tool.operation_id = operation.id
               LEFT JOIN operation_events AS terminal
                 ON terminal.operation_id = operation.id AND terminal.terminal = 1
               WHERE operation.thread_id IN ("#,
        );
        push_string_binds(&mut query, thread_ids);
        query
            .push(") AND operation.started_at_ms < ")
            .push_bind(range.end_ms())
            .push(" AND (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms > ")
            .push_bind(range.start_ms())
            .push(')');
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        let mut wrappers = Vec::new();
        let mut groups_with_nested = HashSet::new();
        let mut counts = ToolCounts::default();
        for row in rows {
            let operation_id: String = row.get("id");
            let group: Option<String> = row.get("execution_group_id");
            if group
                .as_deref()
                .is_some_and(|value| crate::ToolExecutionGroupId::from_string(value).is_none())
            {
                return Err(UsageStoreError::InvalidFact);
            }
            let role: String = row.get("execution_role");
            match role.as_str() {
                "standalone" => {}
                "wrapper" => {
                    increment(&mut counts.wrappers)?;
                    if group.is_none() {
                        increment(&mut counts.unlinked_wrappers)?;
                    }
                    wrappers.push((operation_id, group));
                }
                "nested" => {
                    increment(&mut counts.nested)?;
                    if let Some(group) = &group {
                        groups_with_nested.insert(group.clone());
                    } else {
                        increment(&mut counts.unlinked_nested)?;
                    }
                }
                _ => return Err(UsageStoreError::InvalidFact),
            }
            increment(&mut counts.raw)?;
        }
        let excluded = wrappers
            .into_iter()
            .filter_map(|(operation_id, group)| {
                group
                    .is_some_and(|group| groups_with_nested.contains(&group))
                    .then_some(operation_id)
            })
            .collect();
        Ok((excluded, counts))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn task_tree_tokens(
        &self,
        thread_ids: &[String],
        range: UtcTimeRange,
        operations: &[OperationRow],
        operation_by_id: &HashMap<String, OperationRow>,
        rework: &HashMap<String, bool>,
        agent_effort: &mut BTreeMap<Option<String>, EffortAccumulator>,
        first_pass: &mut EffortAccumulator,
        post_integration_rework: &mut EffortAccumulator,
    ) -> Result<(), UsageStoreError> {
        let selected = operation_by_id.keys().collect::<HashSet<_>>();
        let request_owners = operations
            .iter()
            .filter_map(|operation| {
                operation
                    .model_request_id
                    .as_ref()
                    .map(|request| (request.clone(), operation.id.clone()))
            })
            .collect::<HashMap<_, _>>();
        let expected_requests = operations
            .iter()
            .filter(|operation| {
                operation.kind == "model_request" && operation.model_request_id.is_some()
            })
            .filter_map(|operation| operation.model_request_id.clone())
            .collect::<HashSet<_>>();
        let mut query = QueryBuilder::<Sqlite>::new(
            r#"SELECT token.source_event_id, token.token_count, token.coverage_state,
                      direct_request.id AS direct_request_id,
                      direct_request.operation_id AS direct_operation_id,
                      tool.id AS tool_id, tool.operation_id AS tool_operation_id,
                      tool.covering_model_request_id,
                      covered_request.operation_id AS covered_operation_id
               FROM token_observations AS token
               LEFT JOIN model_requests AS direct_request
                 ON direct_request.id = token.model_request_id
               LEFT JOIN tool_invocations AS tool ON tool.id = token.tool_invocation_id
               LEFT JOIN model_requests AS covered_request
                 ON covered_request.id = tool.covering_model_request_id
               LEFT JOIN operations AS direct_operation
                 ON direct_operation.id = direct_request.operation_id
               LEFT JOIN operations AS tool_operation ON tool_operation.id = tool.operation_id
               WHERE token.category_path = 'total_tokens'
                 AND token.measurement_provenance = 'provider_reported'
                 AND token.observed_at_ms >= "#,
        );
        query
            .push_bind(range.start_ms())
            .push(" AND token.observed_at_ms < ")
            .push_bind(range.end_ms())
            .push(" AND COALESCE(direct_operation.thread_id, tool_operation.thread_id) IN (");
        push_string_binds(&mut query, thread_ids);
        query.push(')');
        query
            .push(" LIMIT ")
            .push_bind(i64::try_from(MAX_TASK_TREE_FACT_ROWS + 1).unwrap_or(i64::MAX));
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        if rows.len() > MAX_TASK_TREE_FACT_ROWS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        let mut facts = HashMap::<(String, String), TokenFact>::new();
        let mut observed_requests = HashSet::new();
        for row in rows {
            let direct_request: Option<String> = row.get("direct_request_id");
            let covering_request: Option<String> = row.get("covering_model_request_id");
            let tool_id: Option<String> = row.get("tool_id");
            let (owner_key, operation_id) = if let Some(request) = direct_request {
                observed_requests.insert(request.clone());
                (
                    format!("request:{request}"),
                    row.get::<Option<String>, _>("direct_operation_id"),
                )
            } else if let Some(request) = covering_request {
                observed_requests.insert(request.clone());
                (
                    format!("request:{request}"),
                    row.get::<Option<String>, _>("covered_operation_id"),
                )
            } else if let Some(tool) = tool_id {
                (
                    format!("tool:{tool}"),
                    row.get::<Option<String>, _>("tool_operation_id"),
                )
            } else {
                return Err(UsageStoreError::InvalidFact);
            };
            let Some(operation_id) = operation_id else {
                return Err(UsageStoreError::InvalidFact);
            };
            if !selected.contains(&operation_id) {
                continue;
            }
            let key = (owner_key, row.get("source_event_id"));
            let fact = TokenFact {
                operation_id,
                token_count: row.get("token_count"),
                coverage: row.get("coverage_state"),
            };
            if let Some(existing) = facts.insert(key, fact.clone())
                && existing != fact
            {
                return Err(UsageStoreError::InvalidFact);
            }
        }
        for fact in facts.values() {
            let operation = operation_by_id
                .get(&fact.operation_id)
                .ok_or(UsageStoreError::InvalidFact)?;
            add_token(
                &mut agent_effort
                    .entry(operation.agent_id.clone())
                    .or_default()
                    .tokens,
                fact.token_count,
                &fact.coverage,
            )?;
            if rework.get(&fact.operation_id).copied().unwrap_or(false) {
                add_token(
                    &mut post_integration_rework.tokens,
                    fact.token_count,
                    &fact.coverage,
                )?;
            } else {
                add_token(&mut first_pass.tokens, fact.token_count, &fact.coverage)?;
            }
        }
        for request in expected_requests.difference(&observed_requests) {
            let operation_id = request_owners
                .get(request.as_str())
                .ok_or(UsageStoreError::InvalidFact)?;
            let operation = operation_by_id
                .get(operation_id)
                .ok_or(UsageStoreError::InvalidFact)?;
            mark_token_unknown(
                &mut agent_effort
                    .entry(operation.agent_id.clone())
                    .or_default()
                    .tokens,
            )?;
            if rework.get(operation_id).copied().unwrap_or(false) {
                mark_token_unknown(&mut post_integration_rework.tokens)?;
            } else {
                mark_token_unknown(&mut first_pass.tokens)?;
            }
        }
        Ok(())
    }

    pub(super) async fn task_tree_context(
        &self,
        thread_ids: &[String],
        range: UtcTimeRange,
        operations: &[OperationRow],
    ) -> Result<TaskTreeContextSummary, UsageStoreError> {
        let operation_ids = operations
            .iter()
            .map(|operation| operation.id.clone())
            .collect::<HashSet<_>>();
        let expected = operations
            .iter()
            .filter(|operation| {
                operation.kind == "model_request" && operation.model_request_id.is_some()
            })
            .filter_map(|operation| operation.model_request_id.clone())
            .collect::<HashSet<_>>();
        let mut query = QueryBuilder::<Sqlite>::new(
            r#"SELECT context.model_request_id, context.policy_estimated_tokens,
                      context.conversation_estimated_tokens,
                      context.tool_output_estimated_tokens, context.estimator,
                      request.operation_id
               FROM model_request_context_sources AS context
               JOIN model_requests AS request ON request.id = context.model_request_id
               JOIN operations AS operation ON operation.id = request.operation_id
               WHERE context.observed_at_ms >= "#,
        );
        query
            .push_bind(range.start_ms())
            .push(" AND context.observed_at_ms < ")
            .push_bind(range.end_ms())
            .push(" AND operation.thread_id IN (");
        push_string_binds(&mut query, thread_ids);
        query.push(')');
        query
            .push(" LIMIT ")
            .push_bind(i64::try_from(MAX_TASK_TREE_FACT_ROWS + 1).unwrap_or(i64::MAX));
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        if rows.len() > MAX_TASK_TREE_FACT_ROWS {
            return Err(UsageStoreError::TaskTreeTooLarge);
        }
        let mut observed = HashSet::new();
        let mut policy = 0_u64;
        let mut conversation = 0_u64;
        let mut tool_output = 0_u64;
        for row in rows {
            let operation_id: String = row.get("operation_id");
            if row.get::<String, _>("estimator") != MODEL_REQUEST_CONTEXT_ESTIMATOR
                || !operation_ids.contains(&operation_id)
            {
                continue;
            }
            observed.insert(row.get::<String, _>("model_request_id"));
            policy = checked_context_add(policy, row.get("policy_estimated_tokens"))?;
            conversation =
                checked_context_add(conversation, row.get("conversation_estimated_tokens"))?;
            tool_output =
                checked_context_add(tool_output, row.get("tool_output_estimated_tokens"))?;
        }
        Ok(TaskTreeContextSummary {
            estimator: MODEL_REQUEST_CONTEXT_ESTIMATOR,
            observed_requests: usize_to_u64(observed.len())?,
            unknown_requests: usize_to_u64(expected.difference(&observed).count())?,
            sources: vec![
                TaskTreeContextSource {
                    source: "policy",
                    estimated_tokens: policy,
                },
                TaskTreeContextSource {
                    source: "conversation",
                    estimated_tokens: conversation,
                },
                TaskTreeContextSource {
                    source: "tool_output",
                    estimated_tokens: tool_output,
                },
            ],
        })
    }
}

fn add_token(
    aggregate: &mut TokenAccumulator,
    count: Option<i64>,
    coverage: &str,
) -> Result<(), UsageStoreError> {
    aggregate.has_gap |= coverage != "complete";
    match count {
        Some(count) if count >= 0 => {
            aggregate.measured_tokens = aggregate
                .measured_tokens
                .checked_add(count)
                .ok_or(UsageStoreError::AggregateOverflow)?;
        }
        Some(_) => return Err(UsageStoreError::InvalidFact),
        None => increment(&mut aggregate.unknown_observations)?,
    }
    Ok(())
}

fn mark_token_unknown(aggregate: &mut TokenAccumulator) -> Result<(), UsageStoreError> {
    aggregate.has_gap = true;
    increment(&mut aggregate.unknown_observations)
}

fn checked_context_add(current: u64, value: i64) -> Result<u64, UsageStoreError> {
    current
        .checked_add(u64::try_from(value).map_err(|_| UsageStoreError::InvalidFact)?)
        .ok_or(UsageStoreError::AggregateOverflow)
}

pub(super) fn combine_effort(
    agents: &BTreeMap<Option<String>, EffortAccumulator>,
) -> Result<TaskTreeEffort, UsageStoreError> {
    let mut total = EffortAccumulator::default();
    for agent in agents.values() {
        total.operations = total
            .operations
            .checked_add(agent.operations)
            .ok_or(UsageStoreError::AggregateOverflow)?;
        total.model_requests = total
            .model_requests
            .checked_add(agent.model_requests)
            .ok_or(UsageStoreError::AggregateOverflow)?;
        total.tokens.measured_tokens = total
            .tokens
            .measured_tokens
            .checked_add(agent.tokens.measured_tokens)
            .ok_or(UsageStoreError::AggregateOverflow)?;
        total.tokens.unknown_observations = total
            .tokens
            .unknown_observations
            .checked_add(agent.tokens.unknown_observations)
            .ok_or(UsageStoreError::AggregateOverflow)?;
        total.tokens.has_gap |= agent.tokens.has_gap;
        total.intervals.extend_from_slice(&agent.intervals);
        total.unknown_intervals = total
            .unknown_intervals
            .checked_add(agent.unknown_intervals)
            .ok_or(UsageStoreError::AggregateOverflow)?;
    }
    finish_effort(total)
}

pub(super) fn agent_summaries(
    metadata: BTreeMap<String, AgentMetadata>,
    effort: BTreeMap<Option<String>, EffortAccumulator>,
) -> Result<Vec<TaskTreeAgentSummary>, UsageStoreError> {
    let mut summaries = Vec::with_capacity(effort.len());
    for (agent_id, effort) in effort {
        let details = agent_id.as_ref().and_then(|id| metadata.get(id));
        let effort = finish_effort(effort)?;
        summaries.push(TaskTreeAgentSummary {
            agent_id,
            role: details
                .map(|details| details.role.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            operations: effort.operations,
            model_requests: effort.model_requests,
            provider_total_tokens: effort.provider_total_tokens,
            wall_time: effort.wall_time,
        });
    }
    summaries.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    Ok(summaries)
}
