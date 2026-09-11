CREATE TABLE thread_wake_policy_state (
    thread_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    stopped_revision INTEGER NOT NULL DEFAULT 0,
    resumed_revision INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE thread_wake_policies (
    thread_id TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    policy_json TEXT NOT NULL,
    revision INTEGER NOT NULL,
    authorization_ref TEXT NOT NULL,
    PRIMARY KEY(thread_id, scope_json),
    FOREIGN KEY(thread_id) REFERENCES thread_wake_policy_state(thread_id) ON DELETE CASCADE
);

-- Keep independently authorized project alarms distinct in a coalesced batch.
ALTER TABLE event_subscription_pending_wakes RENAME TO old_pending_wakes;
DROP INDEX event_subscription_pending_revision_idx;
CREATE TABLE event_subscription_pending_wakes (
    subscription_id TEXT NOT NULL,
    alarm_key TEXT NOT NULL DEFAULT '',
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
    PRIMARY KEY(subscription_id, alarm_key),
    FOREIGN KEY(subscription_id) REFERENCES event_subscriptions(id) ON DELETE CASCADE
);
INSERT INTO event_subscription_pending_wakes
SELECT subscription_id, '', revision, event_pending, heartbeat_pending, manual_pending,
       event_id, event_source, event_type, event_sequence, event_cursor, event_labels_json,
       event_occurred_at_ms, event_count, heartbeat_due_at_ms, updated_at_ms
FROM old_pending_wakes;
DROP TABLE old_pending_wakes;
CREATE INDEX event_subscription_pending_revision_idx ON event_subscription_pending_wakes(revision);
