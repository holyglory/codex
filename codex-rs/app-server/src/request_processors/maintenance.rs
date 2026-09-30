//! Strict checkpoint selection and parent-first restoration for cooperative upgrades.
use super::ThreadRequestProcessor;
use codex_app_server_transport::daemon_recovery::InterruptedTurn;
use codex_app_server_transport::daemon_recovery::MaintenanceSnapshot;
use codex_app_server_transport::daemon_recovery::RecoverySnapshot;
use codex_core::CodexThread;
use codex_core::MaintenancePause;
use codex_core::MaintenancePauseStatus;
use codex_protocol::ThreadId;
use codex_thread_store::PersistContext;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct PausedThreads {
    pub(crate) threads: BTreeMap<String, (Arc<CodexThread>, MaintenancePause)>,
}

impl PausedThreads {
    pub(crate) fn commit(self) {
        for (_, (_, pause)) in self.threads {
            pause.commit();
        }
    }
}

impl ThreadRequestProcessor {
    pub(crate) async fn pause_for_maintenance(
        &self,
        paused: &mut PausedThreads,
        cancelled: &CancellationToken,
    ) -> Result<(), &'static str> {
        let mut created = self.thread_manager.subscribe_thread_created();
        let mut status = self.subscribe_running_assistant_turn_count();
        loop {
            let mut ready = true;
            let census: std::collections::HashSet<_> = self
                .thread_manager
                .list_thread_ids()
                .await
                .into_iter()
                .collect();
            for id in census.iter().copied() {
                let thread = self
                    .thread_manager
                    .get_thread(id)
                    .await
                    .map_err(|_| "threadChanged")?;
                thread
                    .maintenance_barrier()
                    .await
                    .map_err(|_| "threadUnavailable")?;
                let config = thread.config_snapshot().await;
                if config.ephemeral {
                    if thread.maintenance_has_background_work().await {
                        return Err("backgroundWork");
                    }
                    if !thread.maintenance_is_idle().await {
                        return Err("nonpersistentWork");
                    }
                    continue;
                }
                if let std::collections::btree_map::Entry::Vacant(e) =
                    paused.threads.entry(id.to_string())
                {
                    let pause = thread.request_maintenance_pause().ok_or("alreadyPausing")?;
                    e.insert((thread.clone(), pause));
                }
                let (_, pause) = &paused.threads[&id.to_string()];
                match pause.status() {
                    MaintenancePauseStatus::PersistenceFailed => return Err("persistenceFailed"),
                    MaintenancePauseStatus::BackgroundWork => return Err("backgroundWork"),
                    MaintenancePauseStatus::Requested | MaintenancePauseStatus::Released => {
                        ready &= thread.maintenance_is_idle().await;
                    }
                    MaintenancePauseStatus::Paused => {}
                }
                // An in-flight tool may finish normally before the next receipt.
                // Detached work is only a blocker once the turn is idle or parked.
                if (thread.maintenance_is_idle().await
                    || pause.status() == MaintenancePauseStatus::Paused)
                    && thread.maintenance_has_background_work().await
                {
                    return Err("backgroundWork");
                }
                ready &= !thread.maintenance_has_pending_input().await;
            }
            if ready {
                let current: std::collections::HashSet<_> = self
                    .thread_manager
                    .list_thread_ids()
                    .await
                    .into_iter()
                    .collect();
                if census == current {
                    return Ok(());
                }
                continue;
            }
            // One bounded operation watches its receipts and the existing thread
            // registry. No agent or host-capacity slot is used for this wait.
            let changed = futures::future::select_all(
                paused
                    .threads
                    .values_mut()
                    .map(|(_, pause)| Box::pin(pause.changed())),
            );
            tokio::select! {
                _ = cancelled.cancelled() => return Err("cancelled"),
                _ = created.recv() => {}
                _ = status.changed() => {}
                _ = changed => {}
            }
        }
    }

    pub(crate) async fn maintenance_snapshot(
        &self,
        operation_id: String,
        paused: &PausedThreads,
    ) -> Result<RecoverySnapshot, &'static str> {
        let mut saved = RecoverySnapshot::default();
        let mut parents = BTreeMap::new();
        let mut mailboxes = BTreeMap::new();
        let mut turn_contexts = BTreeMap::new();
        let mut stopped = BTreeMap::new();
        for (id, (thread, pause)) in &paused.threads {
            let thread_id = ThreadId::from_string(id).map_err(|_| "invalidThread")?;
            let current = self
                .thread_manager
                .get_thread(thread_id)
                .await
                .map_err(|_| "threadChanged")?;
            if !Arc::ptr_eq(thread, &current) {
                return Err("threadChanged");
            }
            if thread.maintenance_has_background_work().await {
                return Err("backgroundWork");
            }
            if thread.maintenance_has_pending_input().await {
                return Err("pendingInput");
            }
            let config = thread.config_snapshot().await;
            if config.ephemeral {
                return Err("nonpersistentWork");
            }
            let interrupted = thread.interrupted_turn().await;
            if let Some(stopped_turn) = thread.maintenance_stopped_turn().await {
                stopped.insert(id.clone(), stopped_turn);
            }
            if !thread.maintenance_is_idle().await
                && (pause.status() != MaintenancePauseStatus::Paused || interrupted.is_none())
            {
                return Err("workNotPaused");
            }
            let thread_id = ThreadId::from_string(id).map_err(|_| "invalidThread")?;
            self.thread_store
                .persist_thread(thread_id, PersistContext::Standard)
                .await
                .map_err(|_| "persistenceFailed")?;
            thread
                .flush_rollout()
                .await
                .map_err(|_| "persistenceFailed")?;
            let mail = thread.maintenance_mailbox().await;
            if thread.maintenance_is_idle().await
                && mail.iter().any(|mail| mail.communication.trigger_turn)
            {
                return Err("pendingTrigger");
            }
            if !mail.is_empty() {
                mailboxes.insert(id.clone(), mail);
            }
            saved.loaded.insert(id.clone());
            parents.insert(id.clone(), config.parent_thread_id.map(|id| id.to_string()));
            if let Some((turn_id, options, environment)) = interrupted {
                turn_contexts.insert(
                    id.clone(),
                    thread
                        .maintenance_turn_context()
                        .await
                        .ok_or("turnChanged")?,
                );
                saved.interrupted.insert(
                    id.clone(),
                    InterruptedTurn {
                        turn_id,
                        output_schema: options.final_output_json_schema,
                        service_tier: options.service_tier,
                        cyber_access_program: options.cyber_access_program,
                        local_environment: Some((&environment).into()),
                    },
                );
            }
        }
        if parents
            .values()
            .flatten()
            .any(|parent| !parents.contains_key(parent))
        {
            return Err("missingParent");
        }
        saved.maintenance = Some(MaintenanceSnapshot {
            operation_id,
            source_pid: std::process::id(),
            parents,
            mailboxes,
            turn_contexts,
            stopped,
        });
        Ok(saved)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "each saved turn must serialize dispatch with a concurrent Stop acknowledgment"
    )]
    pub(crate) async fn resume_maintenance_turns(
        &self,
        turns: Vec<(String, InterruptedTurn, codex_core::MaintenanceTurnContext)>,
        resumptions: &crate::maintenance_resumption::MaintenanceResumption,
    ) -> Result<(), &'static str> {
        let mut prepared = Vec::new();
        for (id, saved, context) in turns {
            if let Some(continuation) = self.prepare_daemon_continuation(&id, saved).await? {
                prepared.push((id, continuation, context));
            } else if let Some(pending) = resumptions.get(&id) {
                *pending.pending.lock().await =
                    crate::maintenance_resumption::ResumeState::Finished;
            }
        }
        for (id, continuation, context) in prepared {
            use crate::maintenance_resumption::ResumeState;
            let pending = resumptions.get(&id).ok_or("missingPendingTurn")?;
            let mut dispatch = pending.pending.lock().await;
            if pending.cancelled.is_cancelled() {
                continuation
                    .thread
                    .record_maintenance_stop(continuation.turn_id)
                    .await
                    .map_err(|_| "stopPersistenceFailed")?;
                *dispatch = ResumeState::Cancelled;
                continue;
            }
            let resumed_turn_id = continuation.turn_id.clone();
            match continuation
                .thread
                .resume_maintenance_checkpoint(
                    continuation.turn_id,
                    context.options.clone(),
                    context,
                    &pending.cancelled,
                )
                .await
                .map_err(|_| "continuationFailed")?
            {
                codex_core::TurnInputSubmission::Started { .. } => {
                    *dispatch = ResumeState::Started;
                }
                codex_core::TurnInputSubmission::NotSubmitted {
                    reason:
                        codex_core::NotSubmittedReason::NotIdle
                        | codex_core::NotSubmittedReason::Superseded,
                } => {}
                codex_core::TurnInputSubmission::NotSubmitted { .. }
                | codex_core::TurnInputSubmission::Steered { .. } => {
                    return Err("continuationNotStarted");
                }
            }
            if pending.cancelled.is_cancelled() && continuation.thread.maintenance_is_idle().await {
                continuation
                    .thread
                    .record_maintenance_stop(resumed_turn_id)
                    .await
                    .map_err(|_| "stopPersistenceFailed")?;
            }
            if *dispatch != ResumeState::Started {
                *dispatch = if pending.cancelled.is_cancelled() {
                    ResumeState::Cancelled
                } else {
                    ResumeState::Finished
                };
            }
        }
        self.resume_idle_maintenance_work().await;
        Ok(())
    }

    pub(crate) async fn resume_idle_maintenance_work(&self) {
        for id in self.thread_manager.list_thread_ids().await {
            if let Ok(thread) = self.thread_manager.get_thread(id).await {
                thread
                    .emit_thread_idle_lifecycle_if_idle(
                        codex_extension_api::ThreadIdleCause::Completed,
                    )
                    .await;
            }
        }
    }

    pub(crate) async fn restore_maintenance_threads(
        &self,
        mut saved: RecoverySnapshot,
    ) -> Result<Vec<(String, InterruptedTurn, codex_core::MaintenanceTurnContext)>, &'static str>
    {
        let mut maintenance = saved.maintenance.take().ok_or("missingMaintenance")?;
        let mut pending = maintenance.parents;
        let mut loaded = std::collections::BTreeSet::new();
        let mut restore_order = Vec::new();
        while !pending.is_empty() {
            let ready: Vec<_> = pending
                .iter()
                .filter(|(_, parent)| parent.as_ref().is_none_or(|parent| loaded.contains(parent)))
                .map(|(id, parent)| (id.clone(), parent.clone()))
                .collect();
            if ready.is_empty() {
                return Err("invalidAgentTree");
            }
            for (id, parent) in ready {
                if let Some(parent) = parent {
                    let thread_id = ThreadId::from_string(&id).map_err(|_| "invalidThread")?;
                    self.thread_manager
                        .ensure_maintenance_child_loaded(
                            thread_id,
                            ThreadId::from_string(&parent).map_err(|_| "invalidParent")?,
                        )
                        .await
                        .map_err(|_| "childRestoreFailed")?;
                } else {
                    self.thread_resume(
                        super::ThreadResumeTarget::DaemonRecovery(None),
                        codex_app_server_protocol::ThreadResumeParams {
                            thread_id: id.clone(),
                            exclude_turns: true,
                            ..Default::default()
                        },
                        None,
                        None,
                        Default::default(),
                    )
                    .await
                    .map_err(|_| "rootRestoreFailed")?;
                }
                pending.remove(&id);
                restore_order.push(id.clone());
                loaded.insert(id);
            }
        }
        for (id, mail) in maintenance.mailboxes {
            let id = ThreadId::from_string(&id).map_err(|_| "invalidThread")?;
            self.thread_manager
                .get_thread(id)
                .await
                .map_err(|_| "threadUnavailable")?
                .restore_maintenance_mailbox(mail)
                .await
                .map_err(|_| "mailboxRestoreFailed")?;
        }
        for (id, turn_id) in maintenance.stopped {
            let id = ThreadId::from_string(&id).map_err(|_| "invalidStoppedThread")?;
            self.thread_manager
                .get_thread(id)
                .await
                .map_err(|_| "stoppedThreadUnavailable")?
                .record_maintenance_stop(turn_id)
                .await
                .map_err(|_| "stopPersistenceFailed")?;
        }
        // The caller reopens admission only after the complete graph is restored.
        let mut continuations = Vec::new();
        for id in restore_order.into_iter().rev() {
            if let Some(turn) = saved.interrupted.remove(&id) {
                let context = maintenance
                    .turn_contexts
                    .remove(&id)
                    .ok_or("missingMaintenanceContext")?;
                continuations.push((id, turn, context));
            }
        }
        Ok(continuations)
    }
}
