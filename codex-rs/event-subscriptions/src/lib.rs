//! Provider-neutral, durable event subscriptions and shared-deadline scheduling.

mod types;

pub use types::EventFilter;
pub use types::EventMetadata;
pub use types::HeartbeatSpec;
pub use types::ListSubscriptionsQuery;
pub use types::NewSubscription;
pub use types::PendingWakeBatch;
pub use types::PublishEventOutcome;
pub use types::PublishedEvent;
pub use types::SourceCursor;
pub use types::Subscription;
pub use types::SubscriptionPage;
pub use types::TriggerOutcome;
pub use types::ValidationError;
pub use types::WakeBatch;
pub use types::WakeItem;
pub use types::WakeReason;

pub const MAX_EVENT_TYPES: usize = 32;
pub const MAX_LABELS: usize = 16;
pub const MAX_LIST_LIMIT: usize = 100;
pub const MAX_SUBSCRIPTIONS_PER_THREAD: usize = 128;
pub const MAX_TOTAL_SUBSCRIPTIONS: usize = 4_096;
pub const MAX_TRIGGER_SUBSCRIPTIONS: usize = 128;
