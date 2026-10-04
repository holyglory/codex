DROP VIEW effective_classification_events;

-- A malformed or interrupted correction can leave more than one unsuperseded
-- root for an operation. Keep the newest root deterministic for every reader;
-- the append-only raw history remains unchanged.
CREATE VIEW effective_classification_events AS
SELECT classification.*
FROM classification_events AS classification
WHERE NOT EXISTS (
    SELECT 1
    FROM classification_events AS successor
    WHERE successor.supersedes_event_id = classification.event_id
)
AND NOT EXISTS (
    SELECT 1
    FROM classification_events AS newer
    WHERE newer.operation_id = classification.operation_id
      AND NOT EXISTS (
          SELECT 1
          FROM classification_events AS successor
          WHERE successor.supersedes_event_id = newer.event_id
      )
      AND (
          newer.occurred_at_ms > classification.occurred_at_ms
          OR (
              newer.occurred_at_ms = classification.occurred_at_ms
              AND newer.event_id > classification.event_id
          )
      )
);
