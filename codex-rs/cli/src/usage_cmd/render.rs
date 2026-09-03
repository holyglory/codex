use super::error::UsageCommandError;
use super::error::UsageErrorKind;
use codex_usage::DurationAggregate;
use codex_usage::StructuredUsageSummary;
use codex_usage::UsageSummary;
use codex_usage::UsageSummaryScope;
use std::fmt::Write;

pub(crate) fn summary(
    summary: &UsageSummary,
    json_output: bool,
    account: Option<&str>,
) -> Result<String, UsageCommandError> {
    if json_output {
        return serde_json::to_string_pretty(&StructuredUsageSummary::new(
            summary,
            account.map(str::to_string),
        ))
        .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage));
    }

    let mut output = String::new();
    writeln!(output, "Usage scope: {}", human_scope(summary))
        .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    if let Some(account) = account {
        writeln!(output, "Account: {account}")
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    }
    writeln!(
        output,
        "Report schema: {}  Database schema: {}  Taxonomy: {}",
        codex_usage::USAGE_REPORT_SCHEMA_VERSION,
        summary.database_schema_version,
        summary.taxonomy_version,
    )
    .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    writeln!(
        output,
        "Coverage: {} (gaps: {})",
        summary.coverage.overall_state,
        if summary.coverage.has_gaps {
            "yes"
        } else {
            "no"
        }
    )
    .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    writeln!(
        output,
        "Operations: {}  Model requests: {}  Tools: {}",
        summary.operation_count, summary.model_request_count, summary.tool_count
    )
    .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    output.push_str("Provider-native token observations:\n");
    if summary.tokens.is_empty() {
        output.push_str("  none observed\n");
    } else {
        for tokens in &summary.tokens {
            writeln!(
                output,
                "  {} [{}; {}]: measured={} exact={} unknown={} observations={}",
                tokens.category_path,
                tokens.repository_bucket,
                tokens.measurement_provenance,
                tokens.measured_tokens,
                tokens
                    .exact_tokens
                    .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
                tokens.unknown_observations,
                tokens.observation_count,
            )
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
        }
    }
    writeln!(
        output,
        "Tools: count={} duration={} outcomes={}",
        summary.tools.count,
        human_duration(&summary.tools.duration),
        summary
            .tools
            .outcomes
            .iter()
            .map(|outcome| format!("{}={}", outcome.outcome, outcome.count))
            .collect::<Vec<_>>()
            .join(", ")
    )
    .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    writeln!(
        output,
        "Time: request-to-delivery={} execution-union={} summed-agent-active={}",
        human_duration(&summary.timing.request_to_delivery_wall),
        human_duration(&summary.timing.execution_wall_union),
        human_duration(&summary.timing.summed_per_agent_active),
    )
    .map_err(|_| UsageCommandError::new(UsageErrorKind::Storage))?;
    output.push_str(
        "Formula: wall, execution, phase/state, agent, and tool durations are separate; provider token categories are not synthesized into a fake total.\n",
    );
    Ok(output)
}

fn human_duration(value: &DurationAggregate) -> String {
    value.exact_ns.map_or_else(
        || format!("{}ns+unknown", value.measured_ns),
        |exact| format!("{exact}ns"),
    )
}

fn human_scope(summary: &UsageSummary) -> String {
    match &summary.scope {
        UsageSummaryScope::All => "all".to_string(),
        UsageSummaryScope::Thread(id) => format!("chat {}", id.as_str()),
        UsageSummaryScope::Repository(id) => format!("repository {}", id.as_str()),
    }
}
