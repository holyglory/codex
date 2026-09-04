CREATE TABLE event_subscriptions (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    source TEXT,
    event_types_json TEXT NOT NULL,
    label_filters_json TEXT NOT NULL,
    cursor_sequence TEXT,
    cursor_value TEXT,
    heartbeat_interval_ms INTEGER,
    next_heartbeat_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (
        (source IS NOT NULL AND event_types_json != '[]')
        OR heartbeat_interval_ms IS NOT NULL
    )
);

CREATE INDEX event_subscriptions_thread_idx
    ON event_subscriptions(thread_id, created_at_ms, id);
CREATE INDEX event_subscriptions_source_idx
    ON event_subscriptions(source) WHERE source IS NOT NULL;
CREATE INDEX event_subscriptions_heartbeat_idx
    ON event_subscriptions(next_heartbeat_at_ms)
    WHERE next_heartbeat_at_ms IS NOT NULL;

CREATE TABLE event_subscription_pending_wakes (
    subscription_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    event_pending INTEGER NOT NULL DEFAULT 0,
    heartbeat_pending INTEGER NOT NULL DEFAULT 0,
    manual_pending INTEGER NOT NULL DEFAULT 0,
    event_id TEXT,
    event_source TEXT,
    event_type TEXT,
    event_sequence TEXT,
    event_cursor TEXT,
    event_labels_json TEXT,
    event_occurred_at_ms INTEGER,
    event_count INTEGER NOT NULL DEFAULT 0,
    heartbeat_due_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL,
    FOREIGN KEY(subscription_id) REFERENCES event_subscriptions(id) ON DELETE CASCADE
);

CREATE INDEX event_subscription_pending_revision_idx
    ON event_subscription_pending_wakes(revision);

CREATE TABLE event_subscription_revisions (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    revision INTEGER NOT NULL
);

INSERT INTO event_subscription_revisions(singleton, revision) VALUES (1, 0);
