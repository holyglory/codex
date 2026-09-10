use super::*;

fn text(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > 512 || value.contains('\0') {
        Err("project values must contain 1..512 non-NUL bytes".into())
    } else {
        Ok(())
    }
}

fn identifier(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value.contains("..")
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        Err(
            "project target and workstream identifiers must be 1..64 ASCII letters, digits or -_."
                .into(),
        )
    } else {
        Ok(())
    }
}

impl ProjectAutomation {
    pub fn apply(
        &mut self,
        thread_id: ThreadId,
        command: ProjectAutomationCommand,
        now_ms: i64,
    ) -> Result<(), String> {
        let records_work = matches!(&command, ProjectAutomationCommand::Bind { purpose, .. } if *purpose != WorkPurpose::Discussion)
            || matches!(
                &command,
                ProjectAutomationCommand::ActivateDelivery { .. }
                    | ProjectAutomationCommand::RecordDelivery { .. }
            );
        match command {
            ProjectAutomationCommand::Status => return Ok(()),
            ProjectAutomationCommand::LinkWork {
                outcome_id,
                experiment_ref,
                clear_outcome,
                clear_experiment,
            } => {
                if !self.threads.contains_key(&thread_id.to_string()) {
                    return Err("bind the task purpose before linking work".into());
                }
                if (outcome_id.is_some() && clear_outcome)
                    || (experiment_ref.is_some() && clear_experiment)
                {
                    return Err("a work association cannot be set and cleared together".into());
                }
                if outcome_id.is_none()
                    && experiment_ref.is_none()
                    && !clear_outcome
                    && !clear_experiment
                {
                    return Err("provide a work association or an explicit clear operation".into());
                }
                for value in outcome_id.iter().chain(experiment_ref.iter()) {
                    text(value)?;
                    if value.len() > 256 {
                        return Err("work association references must fit within 256 bytes".into());
                    }
                }
                if outcome_id.as_ref().is_some_and(|value| {
                    !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
                }) {
                    return Err("outcome links require a content-free record identifier".into());
                }
                if let Some(value) = &experiment_ref {
                    let (record, revision) = value
                        .split_once('@')
                        .ok_or("experiment links require RECORD@REVISION")?;
                    let parsed = revision
                        .parse::<u32>()
                        .map_err(|_| "invalid experiment revision")?;
                    if record.is_empty()
                        || !record
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
                        || parsed == 0
                        || parsed.to_string() != revision
                    {
                        return Err("experiment links require a record identifier and positive canonical revision".into());
                    }
                }
                if let Some(value) = outcome_id {
                    self.thread_outcomes.insert(thread_id.to_string(), value);
                }
                if clear_outcome {
                    self.thread_outcomes.remove(&thread_id.to_string());
                }
                if let Some(value) = experiment_ref {
                    self.thread_experiments.insert(thread_id.to_string(), value);
                }
                if clear_experiment {
                    self.thread_experiments.remove(&thread_id.to_string());
                }
            }
            ProjectAutomationCommand::Bind {
                purpose,
                workstream,
            } => {
                if self.completed {
                    self.completed = false;
                    self.paused = false;
                    self.owner_thread_id = thread_id;
                    self.threads.clear();
                    self.thread_workstreams.clear();
                    self.thread_outcomes.clear();
                    self.thread_experiments.clear();
                    self.delivery.clear();
                    self.implementation_starts.clear();
                    self.review_window_start_ms = now_ms;
                    self.next_review_at_ms = now_ms.saturating_add(self.review_interval_ms);
                }
                if self.threads.len() >= 256 && !self.threads.contains_key(&thread_id.to_string()) {
                    return Err("project task capacity reached".into());
                }
                if let Some(workstream) = workstream {
                    identifier(&workstream)?;
                    self.thread_workstreams
                        .insert(thread_id.to_string(), workstream);
                } else {
                    self.thread_workstreams
                        .entry(thread_id.to_string())
                        .or_insert_with(|| "default".into());
                }
                self.threads.insert(thread_id.to_string(), purpose);
                if purpose == WorkPurpose::Implementation {
                    let workstream = self.thread_workstreams[&thread_id.to_string()].clone();
                    self.implementation_starts
                        .entry(workstream)
                        .or_insert(now_ms);
                }
            }
            ProjectAutomationCommand::ActivateDelivery {
                target,
                surface,
                acceptance,
                delivery_interval_ms,
                hard_stop_interval_ms,
            } => {
                identifier(&target)?;
                text(&surface)?;
                text(&acceptance)?;
                if self.threads.get(&thread_id.to_string()) != Some(&WorkPurpose::Implementation) {
                    return Err("only implementation work with a meaningful authorized preliminary result may activate delivery".into());
                }
                if self.delivery.contains_key(&target) {
                    return Err("target already exists; use an explicit postponement instead of resetting its baseline".into());
                }
                if self.delivery.len() >= MAX_PROJECT_TARGETS {
                    return Err("delivery target capacity reached".into());
                }
                let delivery_interval_ms = delivery_interval_ms.unwrap_or(DAY_MS);
                let hard_stop_interval_ms = hard_stop_interval_ms.unwrap_or(DAY_MS + DAY_MS / 2);
                if delivery_interval_ms <= 0 || hard_stop_interval_ms < delivery_interval_ms {
                    return Err(
                        "intervals must be positive and the hard stop must not precede delivery"
                            .into(),
                    );
                }
                let workstream = self
                    .thread_workstreams
                    .get(&thread_id.to_string())
                    .cloned()
                    .unwrap_or_else(|| "default".into());
                let started_at_ms = self
                    .implementation_starts
                    .get(&workstream)
                    .copied()
                    .unwrap_or(now_ms);
                self.delivery.insert(
                    target.clone(),
                    DeliveryObligation {
                        target,
                        workstream,
                        surface,
                        acceptance,
                        started_at_ms,
                        delivered_at_ms: None,
                        delivery_interval_ms,
                        hard_stop_interval_ms,
                        delivery_due_at_ms: started_at_ms
                            .checked_add(delivery_interval_ms)
                            .ok_or("delivery deadline overflow")?,
                        hard_stop_at_ms: started_at_ms
                            .checked_add(hard_stop_interval_ms)
                            .ok_or("hard-stop deadline overflow")?,
                        revision: 1,
                        paused: false,
                        job: None,
                        evidence_ref: None,
                    },
                );
            }
            ProjectAutomationCommand::Postpone {
                target,
                delivery_due_at_ms,
                hard_stop_at_ms,
                authorization_ref,
            } => {
                text(&authorization_ref)?;
                if delivery_due_at_ms <= 0 || hard_stop_at_ms < delivery_due_at_ms {
                    return Err("invalid postponed deadlines".into());
                }
                let obligation = self
                    .delivery
                    .get_mut(&target)
                    .ok_or("unknown delivery target")?;
                obligation.delivery_due_at_ms = delivery_due_at_ms;
                obligation.hard_stop_at_ms = hard_stop_at_ms;
                obligation.revision += 1;
                obligation.job = None;
            }
            ProjectAutomationCommand::Pause {
                target,
                authorization_ref,
            } => {
                text(&authorization_ref)?;
                match target {
                    Some(target) => {
                        self.delivery
                            .get_mut(&target)
                            .ok_or("unknown delivery target")?
                            .paused = true
                    }
                    None => self.paused = true,
                }
            }
            ProjectAutomationCommand::Resume { target } => {
                if self.completed {
                    return Err("completed work cannot resume old alarms; bind a new purpose to start new work".into());
                }
                match target {
                    Some(target) => {
                        self.delivery
                            .get_mut(&target)
                            .ok_or("unknown delivery target")?
                            .paused = false
                    }
                    None => self.paused = false,
                }
            }
            ProjectAutomationCommand::RecordDelivery {
                target,
                delivered_at_ms,
                evidence_ref,
            } => {
                text(&evidence_ref)?;
                let obligation = self
                    .delivery
                    .get_mut(&target)
                    .ok_or("unknown delivery target")?;
                if delivered_at_ms > now_ms
                    || delivered_at_ms < obligation.started_at_ms
                    || obligation
                        .delivered_at_ms
                        .is_some_and(|previous| delivered_at_ms <= previous)
                {
                    return Err("delivery time must be a new actual observation within the obligation lifetime".into());
                }
                obligation.delivered_at_ms = Some(delivered_at_ms);
                obligation.delivery_due_at_ms = delivered_at_ms
                    .checked_add(obligation.delivery_interval_ms)
                    .ok_or("delivery deadline overflow")?;
                obligation.hard_stop_at_ms = delivered_at_ms
                    .checked_add(obligation.hard_stop_interval_ms)
                    .ok_or("hard-stop deadline overflow")?;
                obligation.evidence_ref = Some(evidence_ref);
                obligation.revision += 1;
                obligation.job = None;
            }
            ProjectAutomationCommand::CompleteReview {
                job_id,
                decision_ref,
            } => {
                text(&decision_ref)?;
                let job = self.review.as_ref().ok_or("no pending review")?;
                if job.id != job_id {
                    return Err("review job was superseded".into());
                }
                if let Some(signal) = &job.decision_ref {
                    self.reviewed_signals.insert(signal.clone());
                    while self.reviewed_signals.len() > 128 {
                        self.reviewed_signals.pop_first();
                    }
                }
                self.last_review_ref = Some(decision_ref);
                self.review_window_start_ms = job.due_at_ms;
                self.next_review_at_ms = job.due_at_ms.saturating_add(self.review_interval_ms);
                self.review = None;
            }
            ProjectAutomationCommand::RequestReview { evidence_ref } => {
                text(&evidence_ref)?;
                if self.review.is_none() && !self.reviewed_signals.contains(&evidence_ref) {
                    let mut job = self.job(AutomationJobKind::PerformanceReview, now_ms);
                    job.decision_ref = Some(evidence_ref);
                    self.review = Some(job);
                } else if let Some(job) = &mut self.review
                    && job.decision_ref.as_ref() != Some(&evidence_ref)
                    && !self.reviewed_signals.contains(&evidence_ref)
                {
                    job.decision_ref = Some(evidence_ref);
                    job.notified = false;
                }
            }
            ProjectAutomationCommand::Transfer {
                owner_thread_id,
                authorization_ref,
            } => {
                text(&authorization_ref)?;
                if !self.threads.contains_key(&owner_thread_id.to_string()) {
                    return Err("new owner must be bound to the project".into());
                }
                self.owner_thread_id = owner_thread_id;
                if let Some(job) = &mut self.review {
                    job.notified = false;
                }
                for obligation in self.delivery.values_mut() {
                    if let Some(job) = &mut obligation.job {
                        job.notified = false;
                    }
                }
            }
            ProjectAutomationCommand::Complete { outcome_ref } => {
                text(&outcome_ref)?;
                if self.review.is_some()
                    || self
                        .delivery
                        .values()
                        .any(|target| !target.paused && target.delivered_at_ms.is_none())
                {
                    return Err("complete the pending review and deliver or explicitly remove each obligation first".into());
                }
                self.paused = true;
                self.completed = true;
            }
        }
        self.revision += 1;
        if records_work {
            self.last_activity_at_ms = self.last_activity_at_ms.max(now_ms);
        }
        Ok(())
    }
}
