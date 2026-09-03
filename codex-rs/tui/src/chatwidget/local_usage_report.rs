use super::local_usage::LocalUsageQuery;
use super::*;
use codex_app_server_protocol::LocalUsageReport;
use codex_app_server_protocol::LocalUsageReportDuration;

pub(super) fn report_items(
    report: &LocalUsageReport,
    scope_label: &str,
    back: LocalUsageQuery,
) -> Vec<SelectionItem> {
    let mut items = vec![
        fact(
            "Report",
            format!(
                "{} v{} · database schema {} · taxonomy {}",
                report.kind,
                report.schema_version,
                report.database_schema_version,
                report.taxonomy_version,
            ),
        ),
        fact("Scope", format!("{scope_label} ({})", report.scope.kind)),
        fact(
            "Account",
            report
                .account
                .as_deref()
                .unwrap_or("all accounts")
                .to_string(),
        ),
        fact(
            "Time range",
            match report.time_range {
                Some(range) => format!("{} to {} ms UTC", range.start_ms, range.end_ms),
                None => "all collected history".to_string(),
            },
        ),
        fact(
            "Coverage",
            format!(
                "{} · gaps: {}",
                report.coverage.state,
                yes_no(report.coverage.has_gaps),
            ),
        ),
    ];
    push_coverage_counts(&mut items, "Coverage events", &report.coverage.events);
    push_coverage_counts(
        &mut items,
        "Token observations",
        &report.coverage.token_observations,
    );

    items.push(disabled("Provider tokens (provenance first):"));
    if report.provider_tokens.is_empty() {
        items.push(disabled("  unobserved"));
    } else {
        items.extend(report.provider_tokens.iter().map(|tokens| {
            let repository = tokens
                .repository_label
                .as_deref()
                .unwrap_or("collected repository");
            fact(
                &tokens.category,
                format!(
                    "provenance {} · repository {} · measured {} · exact {} · unknown {} · observations {}",
                    tokens.measurement_provenance,
                    repository,
                    tokens.measured_tokens,
                    optional_i64(tokens.exact_tokens),
                    tokens.unknown_observations,
                    tokens.observation_count,
                ),
            )
        }));
    }
    items.push(fact(
        "Counts",
        format!(
            "operations {} · model requests {} · tools {}",
            report.counts.operations, report.counts.model_requests, report.counts.tools,
        ),
    ));

    items.push(disabled("Provider tokens by activity (provenance first):"));
    if report.provider_tokens_by_activity.is_empty() {
        items.push(disabled("  unobserved"));
    } else {
        items.extend(report.provider_tokens_by_activity.iter().map(|tokens| {
            fact(
                &format!("{}/{}", tokens.phase, tokens.activity),
                format!(
                    "provenance {} · measured {} · exact {} · unknown {}",
                    tokens.attribution_provenance,
                    tokens.measured_tokens,
                    optional_i64(tokens.exact_tokens),
                    tokens.unknown_observations,
                ),
            )
        }));
    }

    items.push(fact(
        "Tools",
        format!(
            "count {} · duration basis {} · {}",
            report.tools.count,
            report.tools.duration_basis,
            duration(&report.tools.duration),
        ),
    ));
    if report.tools.outcomes.is_empty() {
        items.push(disabled("  outcomes: unobserved"));
    } else {
        items.extend(report.tools.outcomes.iter().map(|outcome| {
            fact(
                &format!("Outcome · {}", outcome.outcome),
                outcome.count.to_string(),
            )
        }));
    }

    items.push(disabled("Time and concurrency:"));
    items.push(fact(
        "Request-to-delivery wall",
        duration(&report.time.request_to_delivery_wall),
    ));
    items.push(fact(
        "Execution wall union",
        duration(&report.time.execution_wall_union),
    ));
    items.push(fact(
        "Summed per-agent active",
        duration(&report.time.summed_per_agent_active),
    ));
    push_named_durations(
        &mut items,
        "Phase interval unions",
        &report.time.phase_interval_unions,
    );
    push_named_durations(
        &mut items,
        "Activity-state interval unions",
        &report.time.activity_state_interval_unions,
    );

    items.push(disabled("Classifications (provenance first):"));
    if report.classifications.is_empty() {
        items.push(disabled("  unobserved"));
    } else {
        items.extend(report.classifications.iter().map(|classification| {
            fact(
                &format!("{}/{}", classification.phase, classification.activity),
                format!(
                    "provenance {} · count {}",
                    classification.provenance, classification.count,
                ),
            )
        }));
    }

    items.push(fact(
        "Repository participation",
        format!(
            "operations {} · tools {} · additive {} · {}",
            report.repository_participation.operation_count,
            report.repository_participation.tool_count,
            yes_no(report.repository_participation.additive),
            report.repository_participation.label,
        ),
    ));
    items.push(fact(
        "Formula · wall time",
        report.formulas.wall_time.clone(),
    ));
    items.push(fact("Formula · tokens", report.formulas.tokens.clone()));
    items.push(fact(
        "Formula · concurrency",
        report.formulas.concurrency.clone(),
    ));
    items.push(fact(
        "Formula · repository",
        report.formulas.repository.clone(),
    ));
    items.push(SelectionItem {
        name: "Close".to_string(),
        dismiss_on_select: true,
        ..Default::default()
    });
    make_facts_selectable(&mut items, &back);
    items
}

fn push_coverage_counts(
    items: &mut Vec<SelectionItem>,
    label: &str,
    counts: &[codex_app_server_protocol::LocalUsageReportCoverageCount],
) {
    if counts.is_empty() {
        items.push(disabled(&format!("{label}: unobserved")));
        return;
    }
    items.push(disabled(&format!("{label}:")));
    items.extend(
        counts
            .iter()
            .map(|count| fact(&count.state, count.count.to_string())),
    );
}

fn push_named_durations(
    items: &mut Vec<SelectionItem>,
    label: &str,
    durations: &[codex_app_server_protocol::LocalUsageReportNamedDuration],
) {
    if durations.is_empty() {
        items.push(disabled(&format!("{label}: unobserved")));
        return;
    }
    items.push(disabled(&format!("{label}:")));
    items.extend(
        durations
            .iter()
            .map(|item| fact(&item.name, duration(&item.duration))),
    );
}

fn duration(value: &LocalUsageReportDuration) -> String {
    format!(
        "measured {} ns · exact {} ns · unknown intervals {}",
        value.measured_ns,
        value
            .exact_ns
            .map_or("unknown".to_string(), |exact| exact.to_string()),
        value.unknown_intervals,
    )
}

fn optional_i64(value: Option<i64>) -> String {
    value.map_or("unknown".to_string(), |value| value.to_string())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn disabled(name: &str) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        is_disabled: true,
        ..Default::default()
    }
}

fn fact(name: &str, description: String) -> SelectionItem {
    SelectionItem {
        name: name.to_string(),
        description: Some(description),
        is_disabled: true,
        ..Default::default()
    }
}

fn make_facts_selectable(items: &mut [SelectionItem], back: &LocalUsageQuery) {
    for item in items {
        if !item.is_disabled {
            continue;
        }
        let Some(detail) = item.description.clone() else {
            continue;
        };
        let title = item.name.clone();
        let back = back.clone();
        item.is_disabled = false;
        item.actions = vec![Box::new(move |tx| {
            tx.send(AppEvent::ShowLocalUsageFactDetail {
                title: title.clone(),
                detail: detail.clone(),
                back: back.clone(),
            });
        })];
        item.dismiss_on_select = true;
    }
}
