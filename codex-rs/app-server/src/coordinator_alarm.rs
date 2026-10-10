//! Versioned Coordinator contract. No Coordinator code or database is linked here.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewReminder {
    version: u8,
    reminder_id: String,
    repository_id: String,
    workstream_id: Option<String>,
    owner_thread_id: String,
    alarm_namespace: String,
    window_start_ms: i64,
    window_end_ms: i64,
    due_at_ms: i64,
    last_completed_receipt: Option<String>,
    escalation: bool,
}
impl ReviewReminder {
    pub(super) fn event(
        self,
        cursor: u64,
        occurred_at: &str,
    ) -> Result<PublishedEvent, SourceError> {
        if self.version != 1
            || self.window_start_ms < 0
            || self.window_end_ms <= self.window_start_ms
            || self.due_at_ms < self.window_end_ms
            || (!self.escalation && self.due_at_ms != self.window_end_ms)
        {
            return Err(SourceError::InvalidEnvelope);
        }
        let mut labels = BTreeMap::from([
            ("reminder_id".into(), self.reminder_id),
            ("repository_id".into(), self.repository_id),
            ("owner_thread_id".into(), self.owner_thread_id),
            ("alarm_namespace".into(), self.alarm_namespace),
            ("window_start_ms".into(), self.window_start_ms.to_string()),
            ("window_end_ms".into(), self.window_end_ms.to_string()),
            ("escalation".into(), self.escalation.to_string()),
            ("due_at_ms".into(), self.due_at_ms.to_string()),
        ]);
        if let Some(scope) = self.workstream_id {
            labels.insert("workstream_id".into(), scope);
        }
        if let Some(receipt) = self.last_completed_receipt {
            labels.insert("last_completed_receipt".into(), receipt);
        }
        let event = PublishedEvent {
            id: cursor.to_string(),
            source: "devcoordinator".into(),
            event_type: "review.reminder".into(),
            cursor: SourceCursor {
                sequence: cursor,
                value: None,
            },
            labels,
            occurred_at_ms: DateTime::parse_from_rfc3339(occurred_at)
                .map_err(|_| SourceError::InvalidEnvelope)?
                .timestamp_millis(),
        };
        event.validate().map_err(|_| SourceError::InvalidEnvelope)?;
        Ok(event)
    }
}

pub(super) async fn deliver(
    store: &SqliteEventSubscriptionStore,
    service: &EventSubscriptionService,
    subscriptions: &[Subscription],
    event: &PublishedEvent,
) -> Result<(), SourceError> {
    for subscription in subscriptions {
        if !subscription
            .filter
            .as_ref()
            .is_some_and(|filter| filter.matches(event))
        {
            continue;
        }
        let labels = &event.labels;
        let spec = codex_event_subscriptions::AlarmSpec {
            dedupe_key: format!("{}:{}", labels["alarm_namespace"], labels["reminder_id"]),
            project_id: Some(labels["repository_id"].clone()),
            workstream_id: labels.get("workstream_id").cloned(),
            subject: if labels["escalation"] == "true" {
                "Performance review escalation"
            } else {
                "Performance review due"
            }
            .into(),
            summary: format!(
                "Coordinator review window [{},{}). Read review.policy.status first; acknowledge inactive, transferred or already-covered reminders without repeating the review. Inspect usage_stats performance_review with from_at_ms={} and to_at_ms={}; use DevCoordinator2 review.prepare then review.record. Last completed receipt: {}. Acknowledge this local delivery with alarm_ack(alarm_id). A reminder is not a review receipt. Act before unrelated work; an explicit user override permits continued work while the review remains outstanding.",
                labels["window_start_ms"],
                labels["window_end_ms"],
                labels["window_start_ms"],
                labels["window_end_ms"],
                labels
                    .get("last_completed_receipt")
                    .map_or("none", String::as_str)
            ),
            absolute_at_ms: Some(
                labels["due_at_ms"]
                    .parse()
                    .map_err(|_| SourceError::InvalidEnvelope)?,
            ),
            relative_ms: None,
            active_work_ms: None,
            operation_result: None,
            expires_at_ms: None,
        };
        store
            .set_alarm(
                subscription.thread_id,
                spec,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            .map_err(|_| SourceError::Unavailable)?;
        store
            .advance_event_route(subscription.id, event.cursor.sequence)
            .await
            .map_err(|_| SourceError::Unavailable)?;
        service.notify_thread_ready(subscription.thread_id);
    }
    Ok(())
}

#[derive(Deserialize)]
struct PendingEnvelope {
    protocol: u8,
    ok: bool,
    data: Option<Pending>,
}
#[derive(Deserialize)]
struct Pending {
    cursor: u64,
    reminders: Vec<ReviewReminder>,
    next_after_id: Option<u64>,
}

// This bounded reconciliation runs inside the existing bridge, never a second watcher.
pub(super) async fn reconcile(
    cancellation: &CancellationToken,
) -> Result<Option<SourceBatch>, SourceError> {
    let mut after = 0;
    let mut events = Vec::new();
    let mut head = 0;
    loop {
        let mut command = Command::new("devcoordinator2");
        command.env_remove("DEVCOORDINATOR_WORK_CONTEXT");
        command.args([
            "review",
            "delivery-pending",
            "--alarm-namespace",
            "codex.review.v1",
            "--after-id",
            &after.to_string(),
        ]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        {
            let parent_pid =
                i32::try_from(std::process::id()).map_err(|_| SourceError::Unavailable)?;
            unsafe {
                command.pre_exec(move || {
                    codex_utils_pty::process_group::set_parent_death_signal(parent_pid)
                });
            }
        }
        #[cfg(windows)]
        let job = codex_utils_pty::JobObject::create().map_err(|_| SourceError::Unavailable)?;
        #[cfg(windows)]
        let mut child = job
            .spawn_contained(&mut command)
            .map_err(|_| SourceError::Unavailable)?;
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(|_| SourceError::Unavailable)?;
        let output = async {
            let stdout = child.stdout.take().ok_or(SourceError::Unavailable)?;
            let mut bytes = Vec::new();
            stdout
                .take((MAX_ENVELOPE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| SourceError::Unavailable)?;
            if bytes.len() > MAX_ENVELOPE_BYTES {
                return Err(SourceError::InvalidEnvelope);
            }
            let status = child.wait().await.map_err(|_| SourceError::Unavailable)?;
            if !status.success() {
                return Ok(None);
            }
            let envelope: PendingEnvelope =
                serde_json::from_slice(&bytes).map_err(|_| SourceError::InvalidEnvelope)?;
            if envelope.protocol != 2 {
                return Err(SourceError::InvalidEnvelope);
            }
            Ok(if envelope.ok { envelope.data } else { None })
        };
        let result = tokio::select! {
            _=cancellation.cancelled()=>Err(SourceError::Cancelled),
            result=tokio::time::timeout(Duration::from_secs(10),output)=>result.map_err(|_|SourceError::Unavailable)?,
        };
        if child.id().is_some() {
            let _ = child.kill().await;
        }
        let Some(page) = result? else {
            return Ok(None);
        };
        if page.reminders.len() > 16 || events.len() + page.reminders.len() > 4096 {
            return Err(SourceError::InvalidEnvelope);
        }
        // Keep the first page watermark so events published during pagination are replayed.
        if after == 0 {
            head = page.cursor;
        }
        for reminder in page.reminders {
            let at = DateTime::from_timestamp_millis(reminder.due_at_ms)
                .ok_or(SourceError::InvalidEnvelope)?
                .to_rfc3339();
            events.push(reminder.event(head, &at)?);
        }
        match page.next_after_id {
            Some(next) if next > after => after = next,
            Some(_) => return Err(SourceError::InvalidEnvelope),
            None => {
                return Ok(Some(SourceBatch {
                    cursor: head,
                    events,
                }));
            }
        }
    }
}
