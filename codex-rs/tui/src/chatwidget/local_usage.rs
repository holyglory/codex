use super::*;
use crate::bottom_pane::SelectionDescriptionLayout;
use codex_app_server_protocol::LocalUsageActivity;
use codex_app_server_protocol::LocalUsageActivityListResponse;
use codex_app_server_protocol::LocalUsageCoverage;
use codex_app_server_protocol::LocalUsageEventListResponse;
use codex_app_server_protocol::LocalUsageRepositoryListResponse;
use codex_app_server_protocol::LocalUsageRepositoryReadResponse;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::LocalUsageThreadReadResponse;
use codex_app_server_protocol::LocalUsageTool;
use codex_app_server_protocol::LocalUsageToolListResponse;

const LOCAL_USAGE_VIEW_ID: &str = "local-usage";

fn local_usage_hint_line() -> Line<'static> {
    Line::from(vec![
        key_hint::plain(KeyCode::Enter).into(),
        " select · ".into(),
        key_hint::plain(KeyCode::Esc).into(),
        " back".into(),
    ])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LocalUsageQuery {
    All,
    Chat {
        thread_id: String,
    },
    Repositories {
        cursor: Option<String>,
    },
    Repository {
        repository_key: String,
    },
    Tools {
        thread_id: String,
        cursor: Option<String>,
    },
    Activities {
        thread_id: String,
        cursor: Option<String>,
    },
    Events {
        thread_id: String,
        cursor: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LocalUsageResponse {
    Summary(LocalUsageSummaryResponse),
    Chat(LocalUsageThreadReadResponse),
    Repositories(LocalUsageRepositoryListResponse),
    Repository(LocalUsageRepositoryReadResponse),
    Tools(LocalUsageToolListResponse),
    Activities(LocalUsageActivityListResponse),
    Events(LocalUsageEventListResponse),
}

impl ChatWidget {
    pub(crate) fn set_local_usage_supported(&mut self, supported: bool) {
        self.local_usage_supported = supported;
        self.bottom_pane
            .set_token_activity_command_enabled(self.has_codex_backend_auth || supported);
    }

    pub(crate) fn local_usage_supported(&self) -> bool {
        self.local_usage_supported
    }

    pub(super) fn open_local_usage_command(&mut self, command: &str) {
        if !self.local_usage_supported {
            self.add_error_message("Usage: /usage [daily|weekly|cumulative]".to_string());
            return;
        }
        let query = match command {
            "all" => LocalUsageQuery::All,
            "repo" => LocalUsageQuery::Repositories { cursor: None },
            "chat" | "tools" | "activities" | "events" => {
                let Some(thread_id) = self.thread_id else {
                    self.add_error_message(
                        "Local usage is unavailable until the chat has started.".to_string(),
                    );
                    return;
                };
                match command {
                    "chat" => LocalUsageQuery::Chat {
                        thread_id: thread_id.to_string(),
                    },
                    "tools" => LocalUsageQuery::Tools {
                        thread_id: thread_id.to_string(),
                        cursor: None,
                    },
                    "activities" => LocalUsageQuery::Activities {
                        thread_id: thread_id.to_string(),
                        cursor: None,
                    },
                    "events" => LocalUsageQuery::Events {
                        thread_id: thread_id.to_string(),
                        cursor: None,
                    },
                    _ => return,
                }
            }
            _ => {
                self.add_error_message(
                    "Usage: /usage [all|chat|repo|tools|activities|events|daily|weekly|cumulative]"
                        .to_string(),
                );
                return;
            }
        };
        self.open_local_usage(query);
    }

    pub(crate) fn open_local_usage(&mut self, query: LocalUsageQuery) {
        let request_id = self.next_local_usage_request_id;
        self.next_local_usage_request_id = self.next_local_usage_request_id.wrapping_add(1);
        self.pending_local_usage_request = Some((request_id, query.clone()));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(LOCAL_USAGE_VIEW_ID),
            title: Some(query.title().to_string()),
            items: vec![SelectionItem {
                name: "Loading local usage...".to_string(),
                is_disabled: true,
                ..Default::default()
            }],
            footer_hint: Some(local_usage_hint_line()),
            ..Default::default()
        });
        self.app_event_tx
            .send(AppEvent::RefreshLocalUsage { request_id, query });
        self.request_redraw();
    }

    pub(crate) fn finish_local_usage(
        &mut self,
        request_id: u64,
        query: LocalUsageQuery,
        result: Result<LocalUsageResponse, String>,
    ) {
        if self
            .pending_local_usage_request
            .as_ref()
            .is_none_or(|pending| pending.0 != request_id || pending.1 != query)
        {
            return;
        }
        self.pending_local_usage_request = None;
        let params = match result {
            Ok(response) => response_params(response, query),
            Err(_) => error_params(query),
        };
        if self
            .bottom_pane
            .replace_selection_view_if_present(LOCAL_USAGE_VIEW_ID, params)
        {
            self.request_redraw();
        }
    }

    pub(crate) fn show_local_usage_tool_detail(
        &mut self,
        tool: LocalUsageTool,
        back: LocalUsageQuery,
    ) {
        let mut items = vec![disabled(format!("Status: {}", tool_status(tool.status)))];
        items.push(disabled(format!("Tool: {}", tool.tool_name)));
        items.push(disabled(format!("Family: {}", tool.operation_family)));
        items.push(disabled(format!("Started: {}", tool.started_at)));
        items.push(disabled(format!(
            "Completed: {}",
            tool.completed_at
                .map_or("unknown".to_string(), |value| value.to_string())
        )));
        items.push(back_item(back));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(LOCAL_USAGE_VIEW_ID),
            title: Some("Tool usage detail".to_string()),
            items,
            footer_hint: Some(local_usage_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_local_usage_activity_detail(
        &mut self,
        activity: LocalUsageActivity,
        back: LocalUsageQuery,
    ) {
        let mut items = vec![disabled("Coverage: unavailable for this row".to_string())];
        items.push(disabled(format!("Phase: {}", phase(activity.phase))));
        items.push(disabled(format!(
            "Activity: {}",
            activity_kind(activity.activity)
        )));
        items.push(disabled(format!(
            "State: {}",
            activity_state(activity.state)
        )));
        items.push(disabled(format!(
            "Provenance: {}",
            provenance(activity.provenance)
        )));
        items.push(disabled(format!("Started: {}", activity.started_at)));
        items.push(disabled(format!(
            "Ended: {}",
            activity
                .ended_at
                .map_or("active".to_string(), |value| value.to_string())
        )));
        items.push(back_item(back));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(LOCAL_USAGE_VIEW_ID),
            title: Some("Activity detail".to_string()),
            items,
            footer_hint: Some(local_usage_hint_line()),
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_local_usage_fact_detail(
        &mut self,
        title: String,
        detail: String,
        back: LocalUsageQuery,
    ) {
        self.bottom_pane.show_selection_view(SelectionViewParams {
            view_id: Some(LOCAL_USAGE_VIEW_ID),
            title: Some(title),
            items: vec![fact_detail(detail), back_item(back)],
            footer_hint: Some(local_usage_hint_line()),
            description_layout: SelectionDescriptionLayout::StackBelowWhenNarrow {
                min_description_width: 28,
            },
            ..Default::default()
        });
        self.request_redraw();
    }
}

impl LocalUsageQuery {
    fn title(&self) -> &'static str {
        match self {
            Self::All => "All local statistics",
            Self::Chat { .. } => "Current chat statistics",
            Self::Repositories { .. } | Self::Repository { .. } => "Repository statistics",
            Self::Tools { .. } => "Tool usage",
            Self::Activities { .. } => "Usage activities",
            Self::Events { .. } => "Collector events",
        }
    }
}

fn response_params(response: LocalUsageResponse, query: LocalUsageQuery) -> SelectionViewParams {
    let title = query.title().to_string();
    let items = match response {
        LocalUsageResponse::Summary(response) => {
            super::local_usage_report::report_items(&response.report, "all repositories", query)
        }
        LocalUsageResponse::Chat(response) => {
            super::local_usage_report::report_items(&response.report, "current chat", query)
        }
        LocalUsageResponse::Repository(response) => {
            let mut items = super::local_usage_report::report_items(
                &response.report,
                &response.repository.label,
                query,
            );
            items.push(back_item(LocalUsageQuery::Repositories { cursor: None }));
            items
        }
        LocalUsageResponse::Repositories(response) => {
            let mut items = response
                .data
                .into_iter()
                .map(|repository| SelectionItem {
                    name: repository.label,
                    description: Some(format!(
                        "Coverage: {} · requests {} · tools {}",
                        coverage(repository.aggregate.coverage),
                        repository.aggregate.model_requests,
                        repository.aggregate.tool_calls,
                    )),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenLocalUsage {
                            query: LocalUsageQuery::Repository {
                                repository_key: repository.repository_key.clone(),
                            },
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            if let Some(cursor) = response.next_cursor {
                items.push(SelectionItem {
                    name: "Next page".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenLocalUsage {
                            query: LocalUsageQuery::Repositories {
                                cursor: Some(cursor.clone()),
                            },
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
            if items.is_empty() {
                items.push(disabled("Coverage: unknown".to_string()));
                items.push(disabled("No repository usage observed.".to_string()));
            }
            items
        }
        LocalUsageResponse::Tools(response) => {
            let back = query.clone();
            let mut items = vec![disabled("Coverage: not included in this list".to_string())];
            items.extend(response.data.into_iter().map(|tool| {
                let detail = tool.clone();
                let back = back.clone();
                SelectionItem {
                    name: tool.tool_name,
                    description: Some(format!(
                        "{} · {}",
                        tool_status(tool.status),
                        tool.operation_family
                    )),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::ShowLocalUsageToolDetail {
                            tool: detail.clone(),
                            back: back.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            }));
            if let (LocalUsageQuery::Tools { thread_id, .. }, Some(cursor)) =
                (&query, response.next_cursor)
            {
                let thread_id = thread_id.clone();
                items.push(next_item(LocalUsageQuery::Tools {
                    thread_id,
                    cursor: Some(cursor),
                }));
            }
            if items.len() == 1 {
                items.push(disabled("No tool usage observed.".to_string()));
            }
            items
        }
        LocalUsageResponse::Activities(response) => {
            let back = query.clone();
            let mut items = vec![disabled("Coverage: not included in this list".to_string())];
            items.extend(response.data.into_iter().map(|activity| {
                let detail = activity.clone();
                let back = back.clone();
                SelectionItem {
                    name: activity_kind(activity.activity).to_string(),
                    description: Some(format!(
                        "{} · {}",
                        phase(activity.phase),
                        provenance(activity.provenance)
                    )),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::ShowLocalUsageActivityDetail {
                            activity: detail.clone(),
                            back: back.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            }));
            if let (LocalUsageQuery::Activities { thread_id, .. }, Some(cursor)) =
                (&query, response.next_cursor)
            {
                items.push(next_item(LocalUsageQuery::Activities {
                    thread_id: thread_id.clone(),
                    cursor: Some(cursor),
                }));
            }
            if items.len() == 1 {
                items.push(disabled("No activity observed.".to_string()));
            }
            items
        }
        LocalUsageResponse::Events(response) => {
            let back = query.clone();
            let mut items = vec![disabled("Coverage and provenance by event:".to_string())];
            items.extend(response.data.into_iter().map(|event| {
                detail_item(
                    event_kind(event.kind),
                    format!(
                        "coverage {} · provenance {} · occurred {}",
                        coverage(event.coverage),
                        provenance(event.provenance),
                        event.occurred_at,
                    ),
                    back.clone(),
                )
            }));
            if let (LocalUsageQuery::Events { thread_id, .. }, Some(cursor)) =
                (&query, response.next_cursor)
            {
                items.push(next_item(LocalUsageQuery::Events {
                    thread_id: thread_id.clone(),
                    cursor: Some(cursor),
                }));
            }
            if items.len() == 1 {
                items.push(disabled("No collector events observed.".to_string()));
            }
            items
        }
    };
    SelectionViewParams {
        view_id: Some(LOCAL_USAGE_VIEW_ID),
        title: Some(title),
        items,
        footer_hint: Some(local_usage_hint_line()),
        description_layout: SelectionDescriptionLayout::StackBelowWhenNarrow {
            min_description_width: 28,
        },
        ..Default::default()
    }
}

fn error_params(query: LocalUsageQuery) -> SelectionViewParams {
    let retry = query.clone();
    SelectionViewParams {
        view_id: Some(LOCAL_USAGE_VIEW_ID),
        title: Some(query.title().to_string()),
        items: vec![
            disabled("Coverage: unavailable".to_string()),
            disabled("Local usage statistics could not be loaded.".to_string()),
            SelectionItem {
                name: "Try again".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenLocalUsage {
                        query: retry.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ],
        footer_hint: Some(local_usage_hint_line()),
        ..Default::default()
    }
}

fn disabled(name: String) -> SelectionItem {
    SelectionItem {
        name,
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

fn fact_detail(detail: String) -> SelectionItem {
    SelectionItem {
        name: "Detail".to_string(),
        description: Some(detail),
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

fn detail_item(name: &str, description: String, back: LocalUsageQuery) -> SelectionItem {
    let mut item = fact(name, description);
    make_facts_selectable(std::slice::from_mut(&mut item), &back);
    item
}

fn next_item(query: LocalUsageQuery) -> SelectionItem {
    SelectionItem {
        name: "Next page".to_string(),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::OpenLocalUsage {
                query: query.clone(),
            });
        })],
        dismiss_on_select: true,
        ..Default::default()
    }
}

fn back_item(query: LocalUsageQuery) -> SelectionItem {
    SelectionItem {
        name: "Back".to_string(),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::OpenLocalUsage {
                query: query.clone(),
            });
        })],
        dismiss_on_select: true,
        ..Default::default()
    }
}

fn coverage(value: LocalUsageCoverage) -> &'static str {
    match value {
        LocalUsageCoverage::Complete => "complete",
        LocalUsageCoverage::Partial => "partial",
        LocalUsageCoverage::Unknown => "unknown",
    }
}

fn provenance(value: codex_app_server_protocol::LocalUsageProvenance) -> &'static str {
    use codex_app_server_protocol::LocalUsageProvenance as Value;
    match value {
        Value::ProviderReported => "provider reported",
        Value::RuntimeObserved => "runtime observed",
        Value::AgentDeclared => "agent declared",
        Value::DeterministicClassification => "deterministic",
        Value::InferredClassification => "inferred",
        Value::UserCorrected => "user corrected",
        Value::Imported => "imported",
        Value::Unknown => "unknown",
    }
}

fn phase(value: codex_app_server_protocol::LocalUsagePhase) -> &'static str {
    use codex_app_server_protocol::LocalUsagePhase as Value;
    match value {
        Value::Planning => "planning",
        Value::Implementation => "implementation",
        Value::Testing => "testing",
        Value::Deployment => "deployment",
        Value::Reporting => "reporting",
        Value::Unattributed => "unattributed",
    }
}

fn activity_state(value: codex_app_server_protocol::LocalUsageActivityState) -> &'static str {
    use codex_app_server_protocol::LocalUsageActivityState as Value;
    match value {
        Value::ModelActive => "model active",
        Value::ToolActive => "tool active",
        Value::ExternalWait => "external wait",
        Value::UserWait => "user wait",
        Value::BlockedWait => "blocked wait",
    }
}

fn activity_kind(value: codex_app_server_protocol::LocalUsageActivityKind) -> &'static str {
    use codex_app_server_protocol::LocalUsageActivityKind as Value;
    match value {
        Value::Requirements => "requirements",
        Value::Specification => "specification",
        Value::RepositoryAnalysis => "repository analysis",
        Value::Research => "research",
        Value::Diagnosis => "diagnosis",
        Value::ArchitectureDesign => "architecture design",
        Value::WorkPlanning => "work planning",
        Value::Coding => "coding",
        Value::Configuration => "configuration",
        Value::Refactoring => "refactoring",
        Value::DependencyOrBuildChange => "dependency/build change",
        Value::TestAuthoring => "test authoring",
        Value::DocumentationAuthoring => "documentation authoring",
        Value::DataOrSchemaChange => "data/schema change",
        Value::BuildValidation => "build validation",
        Value::UnitTesting => "unit testing",
        Value::IntegrationTesting => "integration testing",
        Value::BrowserQa => "browser QA",
        Value::CompatibilityTesting => "compatibility testing",
        Value::MigrationRehearsal => "migration rehearsal",
        Value::VerificationReview => "verification review",
        Value::Packaging => "packaging",
        Value::Deployment => "deployment",
        Value::Rollback => "rollback",
        Value::RuntimeOperations => "runtime operations",
        Value::Monitoring => "monitoring",
        Value::UserElaboration => "user elaboration",
        Value::StatusUpdate => "status update",
        Value::CompletionHandoff => "completion handoff",
        Value::ReviewFeedback => "review feedback",
        Value::Coordination => "coordination",
        Value::AccountingOverhead => "accounting overhead",
        Value::Mixed => "mixed",
        Value::Unknown => "unknown",
    }
}

fn tool_status(value: codex_app_server_protocol::LocalUsageToolStatus) -> &'static str {
    use codex_app_server_protocol::LocalUsageToolStatus as Value;
    match value {
        Value::Completed => "completed",
        Value::Failed => "failed",
        Value::Interrupted => "interrupted",
        Value::Rejected => "rejected",
        Value::Unsupported => "unsupported",
        Value::Unknown => "unknown",
    }
}

fn event_kind(value: codex_app_server_protocol::LocalUsageEventKind) -> &'static str {
    use codex_app_server_protocol::LocalUsageEventKind as Value;
    match value {
        Value::ModelRequestStarted => "model request started",
        Value::ModelRequestCompleted => "model request completed",
        Value::ToolStarted => "tool started",
        Value::ToolCompleted => "tool completed",
        Value::ActivityChanged => "activity changed",
        Value::ClassificationCorrected => "classification corrected",
        Value::CoverageGap => "coverage gap",
    }
}
