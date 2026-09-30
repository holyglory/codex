//! Durable ownership of mail transferred by a maintenance checkpoint.
/// Pending agent mail belongs to its conversation, never the usage/event log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceMail {
    pub communication: codex_protocol::protocol::InterAgentCommunication,
    pub options: crate::TurnStartOptions,
}

#[derive(serde::Deserialize)]
struct HistoryIdentity {
    #[serde(rename = "type")]
    record_type: String,
    payload: HistoryPayloadIdentity,
}

#[derive(serde::Deserialize)]
struct HistoryPayloadIdentity {
    #[serde(default)]
    id: Option<codex_protocol::ResponseItemId>,
}

impl crate::CodexThread {
    pub async fn maintenance_mailbox(&self) -> Vec<MaintenanceMail> {
        self.session.input_queue.maintenance_mailbox().await
    }

    pub(crate) async fn restore_saved_maintenance_mailbox(&self) -> std::io::Result<()> {
        let path = self.maintenance_mailbox_path().await;
        let mail = match tokio::fs::read(&path).await {
            Ok(contents) => serde_json::from_slice(&contents).map_err(std::io::Error::other)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        self.restore_maintenance_mailbox(mail).await
    }

    async fn maintenance_mailbox_path(&self) -> codex_utils_absolute_path::AbsolutePathBuf {
        self.session
            .get_config()
            .await
            .codex_home
            .join("maintenance-inbox")
            .join(format!("{}.json", self.session.thread_id))
    }

    /// Restore only mail not already recorded before a prior recovery attempt.
    /// The streaming decoder ignores payload content, including compacted history.
    pub async fn restore_maintenance_mailbox(
        &self,
        mut mail: Vec<MaintenanceMail>,
    ) -> std::io::Result<()> {
        if mail.is_empty() {
            return Ok(());
        }
        let inbox = self.maintenance_mailbox_path().await;
        match tokio::fs::read(&inbox).await {
            Ok(contents) => mail.extend(
                serde_json::from_slice::<Vec<MaintenanceMail>>(&contents)
                    .map_err(std::io::Error::other)?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let path = self
            .rollout_path()
            .ok_or_else(|| std::io::Error::other("missing mailbox history"))?;
        let path = path.clone();
        let wanted: std::collections::BTreeSet<_> =
            mail.iter()
                .map(|entry| {
                    entry.communication.id.clone().ok_or_else(|| {
                        std::io::Error::other("mailbox entry lacks its stable identity")
                    })
                })
                .collect::<std::io::Result<_>>()?;
        let mut seen = tokio::task::spawn_blocking(
            move || -> std::io::Result<std::collections::BTreeSet<_>> {
                let input = std::io::BufReader::new(std::fs::File::open(path)?);
                let mut seen = std::collections::BTreeSet::new();
                for row in
                    serde_json::Deserializer::from_reader(input).into_iter::<HistoryIdentity>()
                {
                    let row = row.map_err(std::io::Error::other)?;
                    let payload =
                        matches!(row.record_type.as_str(), "response_item" | "agent_mail")
                            .then_some(row.payload);
                    if let Some(id) = payload.and_then(|payload| payload.id)
                        && wanted.contains(&id)
                    {
                        seen.insert(id);
                    }
                    if seen.len() == wanted.len() {
                        break;
                    }
                }
                Ok(seen)
            },
        )
        .await
        .map_err(std::io::Error::other)??;
        // Retain only undelivered identities in the durable inbox. Enqueuing is
        // not delivery; an idle queue-only message must survive a second crash.
        mail.retain(|entry| {
            entry
                .communication
                .id
                .as_ref()
                .is_some_and(|id| !seen.contains(id))
        });
        let mut unique = std::collections::BTreeSet::new();
        mail.retain(|entry| unique.insert(entry.communication.id.clone()));
        let saved = serde_json::to_string(&mail).map_err(std::io::Error::other)?;
        let durable = inbox.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = durable.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::path_utils::write_atomically(&durable, &saved)?;
            std::fs::File::open(&durable)?.sync_all()?;
            #[cfg(unix)]
            if let Some(parent) = durable.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)??;
        seen.extend(
            self.maintenance_mailbox()
                .await
                .into_iter()
                .filter_map(|entry| entry.communication.id),
        );
        for entry in mail {
            if entry
                .communication
                .id
                .as_ref()
                .is_some_and(|id| seen.contains(id))
            {
                continue;
            }
            if let Some(id) = entry.communication.id.clone() {
                seen.insert(id);
            }
            self.session
                .input_queue
                .enqueue_mailbox_communication(entry.communication, entry.options)
                .await;
        }
        Ok(())
    }
}
