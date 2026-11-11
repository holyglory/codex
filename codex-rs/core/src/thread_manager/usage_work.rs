use super::ThreadManager;
use crate::UsageWorkAction;
use crate::UsageWorkReceipt;
use codex_protocol::ThreadId;

impl ThreadManager {
    /// Declares work for a loaded thread at the same ordered boundary used by capture.
    /// Identifiers express an explicit declaration, not verified external authority.
    pub async fn usage_work(
        &self,
        thread_id: ThreadId,
        action: UsageWorkAction,
    ) -> Result<UsageWorkReceipt, String> {
        let thread = self
            .state
            .threads
            .read()
            .await
            .get(&thread_id)
            .cloned()
            .ok_or_else(|| "usage_work requires a loaded thread".to_string())?;
        let capture = self
            .state
            .usage_capture
            .as_ref()
            .ok_or_else(|| "usage_work capture unavailable".to_string())?;
        let result = capture.usage_work(thread_id.to_string(), action).await;
        drop(thread);
        result
    }
}
