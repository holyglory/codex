use super::ThreadManager;
use crate::UsageWorkAction;
use crate::UsageWorkBoundaryState;
use crate::UsageWorkBoundaryStatus;
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

    /// Resolves one explicit turn or job boundary without adding metadata to operations.
    ///
    /// A missing binding produces at most one request for the supplied boundary ID. The
    /// caller may record an explicit decision (`continue`, `select`, `new`, `clear`, or
    /// `unknown`) on a later call; the bounded tracker keeps repeated status checks quiet.
    pub async fn usage_work_boundary(
        &self,
        thread_id: ThreadId,
        boundary_id: String,
        state: Option<UsageWorkBoundaryState>,
    ) -> Result<UsageWorkBoundaryStatus, String> {
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
        let result = capture
            .usage_work_boundary(thread_id.to_string(), boundary_id, state)
            .await;
        drop(thread);
        result
    }
}
