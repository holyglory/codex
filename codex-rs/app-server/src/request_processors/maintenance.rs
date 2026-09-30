//! Strict checkpoint selection and parent-first restoration for cooperative upgrades.
use super::ThreadRequestProcessor;
use codex_app_server_transport::daemon_recovery::{InterruptedTurn, MaintenanceSnapshot, RecoverySnapshot};
use codex_core::{CodexThread, MaintenancePause, MaintenancePauseStatus};
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::ThreadId;
use codex_thread_store::PersistContext;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct PausedThreads {
    pub(crate) threads: BTreeMap<String, (Arc<CodexThread>, MaintenancePause)>,
}

impl PausedThreads {
    pub(crate) fn commit(self) {
        for (_, (_, pause)) in self.threads { pause.commit(); }
    }
}

impl ThreadRequestProcessor {
    pub(crate) async fn pause_for_maintenance(
        &self,
        cancelled: &CancellationToken,
    ) -> Result<PausedThreads, &'static str> {
        let mut paused = PausedThreads { threads: BTreeMap::new() };
        let mut created = self.thread_manager.subscribe_thread_created();
        let mut status = self.subscribe_running_assistant_turn_count();
        loop {
            let mut ready = true;
            for id in self.thread_manager.list_thread_ids().await {
                let thread = self.thread_manager.get_thread(id).await.map_err(|_| "threadChanged")?;
                let config = thread.config_snapshot().await;
                if config.ephemeral {
                    if !thread.maintenance_is_idle().await {
                        return Err("nonpersistentWork");
                    }
                    continue;
                }
                if !paused.threads.contains_key(&id.to_string()) {
                    let pause = thread.request_maintenance_pause().ok_or("alreadyPausing")?;
                    paused.threads.insert(id.to_string(), (thread.clone(), pause));
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
                if thread.maintenance_has_background_work().await { return Err("backgroundWork"); }
            }
            if ready { return Ok(paused); }
            // One bounded operation watches its receipts and the existing thread
            // registry. No agent or host-capacity slot is used for this wait.
            let changed = futures::future::select_all(paused.threads.values_mut()
                .map(|(_, pause)| Box::pin(pause.changed())));
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
        for (id, (thread, pause)) in &paused.threads {
            if thread.maintenance_has_background_work().await { return Err("backgroundWork"); }
            let config = thread.config_snapshot().await;
            if config.ephemeral { return Err("nonpersistentWork"); }
            if config.parent_thread_id.is_some() && thread.multi_agent_version() != Some(MultiAgentVersion::V2) {
                return Err("unsupportedAgentTree");
            }
            let interrupted = thread.interrupted_turn().await;
            if !thread.maintenance_is_idle().await
                && (pause.status() != MaintenancePauseStatus::Paused || interrupted.is_none())
            { return Err("workNotPaused"); }
            let thread_id = ThreadId::from_string(id).map_err(|_| "invalidThread")?;
            self.thread_store.persist_thread(thread_id, PersistContext::Standard).await.map_err(|_| "persistenceFailed")?;
            thread.flush_rollout().await.map_err(|_| "persistenceFailed")?;
            saved.loaded.insert(id.clone());
            parents.insert(id.clone(), config.parent_thread_id.map(|id| id.to_string()));
            if let Some((turn_id, options, environment)) = interrupted {
                saved.interrupted.insert(id.clone(), InterruptedTurn {
                    turn_id,
                    output_schema: options.final_output_json_schema,
                    service_tier: options.service_tier,
                    cyber_access_program: options.cyber_access_program,
                    local_environment: Some((&environment).into()),
                });
            }
        }
        if parents.values().flatten().any(|parent| !parents.contains_key(parent)) {
            return Err("missingParent");
        }
        saved.maintenance = Some(MaintenanceSnapshot { operation_id, parents });
        Ok(saved)
    }

    pub(crate) async fn restore_maintenance_threads(&self, mut saved: RecoverySnapshot) -> Result<Vec<(String, InterruptedTurn)>, &'static str> {
        let maintenance = saved.maintenance.take().ok_or("missingMaintenance")?;
        let mut pending = maintenance.parents;
        let mut loaded = std::collections::BTreeSet::new();
        let mut restore_order = Vec::new();
        while !pending.is_empty() {
            let ready: Vec<_> = pending.iter().filter(|(_, parent)| parent.as_ref().is_none_or(|parent| loaded.contains(parent)))
                .map(|(id, parent)| (id.clone(), parent.clone())).collect();
            if ready.is_empty() { return Err("invalidAgentTree"); }
            for (id, parent) in ready {
                if parent.is_some() {
                    let thread_id = ThreadId::from_string(&id).map_err(|_| "invalidThread")?;
                    self.thread_manager.ensure_multi_agent_v2_child_loaded(thread_id).await.map_err(|_| "childRestoreFailed")?;
                } else {
                    self.thread_resume(super::ThreadResumeTarget::DaemonRecovery(None),
                        codex_app_server_protocol::ThreadResumeParams { thread_id: id.clone(), exclude_turns: true, ..Default::default() },
                        None, None, Default::default()).await.map_err(|_| "rootRestoreFailed")?;
                }
                pending.remove(&id);
                restore_order.push(id.clone());
                loaded.insert(id);
            }
        }
        // The caller reopens admission only after the complete graph is restored.
        Ok(restore_order.into_iter().rev().filter_map(|id| saved.interrupted.remove(&id).map(|turn| (id, turn))).collect())
    }
}
