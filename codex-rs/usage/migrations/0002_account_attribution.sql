ALTER TABLE turns ADD COLUMN account_auth_mode TEXT CHECK (
    account_auth_mode IS NULL OR account_auth_mode IN (
        'api_key', 'chatgpt', 'chatgpt_auth_tokens', 'headers',
        'agent_identity', 'personal_access_token', 'bedrock_api_key'
    )
);

ALTER TABLE model_requests ADD COLUMN account_auth_mode TEXT CHECK (
    account_auth_mode IS NULL OR account_auth_mode IN (
        'api_key', 'chatgpt', 'chatgpt_auth_tokens', 'headers',
        'agent_identity', 'personal_access_token', 'bedrock_api_key'
    )
);

CREATE TABLE activity_spans (
    id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL REFERENCES operations(id),
    activity_state TEXT NOT NULL CHECK (
        activity_state IN ('external_wait', 'user_wait', 'blocked_wait')
    ),
    started_at_ms INTEGER NOT NULL,
    UNIQUE(id, operation_id)
) STRICT;

CREATE TABLE activity_span_events (
    event_id TEXT PRIMARY KEY NOT NULL,
    activity_span_id TEXT NOT NULL REFERENCES activity_spans(id),
    event_kind TEXT NOT NULL CHECK (event_kind IN ('heartbeat', 'ended')),
    occurred_at_ms INTEGER NOT NULL,
    UNIQUE(activity_span_id, event_kind, occurred_at_ms)
) STRICT;

CREATE UNIQUE INDEX activity_span_events_one_end
    ON activity_span_events(activity_span_id)
    WHERE event_kind = 'ended';

CREATE TRIGGER activity_spans_no_update BEFORE UPDATE ON activity_spans
BEGIN SELECT RAISE(ABORT, 'usage entities are immutable'); END;
CREATE TRIGGER activity_spans_no_delete BEFORE DELETE ON activity_spans
BEGIN SELECT RAISE(ABORT, 'usage entities cannot be deleted'); END;
CREATE TRIGGER activity_span_events_no_update BEFORE UPDATE ON activity_span_events
BEGIN SELECT RAISE(ABORT, 'usage events are append-only'); END;
CREATE TRIGGER activity_span_events_no_delete BEFORE DELETE ON activity_span_events
BEGIN SELECT RAISE(ABORT, 'usage events cannot be deleted'); END;
CREATE TRIGGER activity_span_events_before_start BEFORE INSERT ON activity_span_events
WHEN NEW.occurred_at_ms < (
    SELECT started_at_ms FROM activity_spans WHERE id = NEW.activity_span_id
)
BEGIN SELECT RAISE(ABORT, 'usage activity span event precedes start'); END;
CREATE TRIGGER activity_span_events_heartbeat_after_end BEFORE INSERT ON activity_span_events
WHEN NEW.event_kind = 'heartbeat' AND EXISTS (
    SELECT 1 FROM activity_span_events
    WHERE activity_span_id = NEW.activity_span_id AND event_kind = 'ended'
)
AND NOT EXISTS (
    SELECT 1 FROM activity_span_events
    WHERE event_id = NEW.event_id
)
BEGIN SELECT RAISE(ABORT, 'usage activity span has already ended'); END;
