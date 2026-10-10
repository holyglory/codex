pub use codex_protocol::provider_usage::PROVIDER_USAGE_CATEGORY_SCHEMA_VERSION;
pub use codex_protocol::provider_usage::ProviderSourceEventKey;
pub use codex_protocol::provider_usage::ProviderTokenCount;
pub use codex_protocol::provider_usage::ProviderUsage;

/// Terminal state reported by a Responses API provider event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResponseStatus {
    /// The provider reported successful completion.
    Completed,
    /// The provider reported terminal failure.
    Failed,
    /// The provider reported terminal incomplete output.
    Incomplete,
}

/// Content-free usage attached to one terminal provider response event.
///
/// `source_event_key` is a versioned fingerprint derived transiently from a provider response ID.
/// The raw response ID is neither retained nor exposed by this observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUsageObservation {
    status: ProviderResponseStatus,
    source_event_key: Option<ProviderSourceEventKey>,
    usage: ProviderUsage,
}

impl ProviderUsageObservation {
    pub(crate) fn new(
        status: ProviderResponseStatus,
        source_event_key: Option<ProviderSourceEventKey>,
        usage: ProviderUsage,
    ) -> Self {
        Self {
            status,
            source_event_key,
            usage,
        }
    }

    /// Provider-reported terminal state for this observation.
    pub fn status(&self) -> ProviderResponseStatus {
        self.status
    }

    /// Redacted stable key for replay reconciliation, when the provider supplied an ID.
    pub fn source_event_key(&self) -> Option<&ProviderSourceEventKey> {
        self.source_event_key.as_ref()
    }

    /// Exact content-free provider usage metadata.
    pub fn usage(&self) -> &ProviderUsage {
        &self.usage
    }
}
