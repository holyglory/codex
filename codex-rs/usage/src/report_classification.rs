//! Reporting view repair; canonical facts and migration checksums are unchanged.

pub(super) const UPGRADE: &str = r#"
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

DROP TRIGGER IF EXISTS _usage_report_classification;
CREATE TRIGGER _usage_report_classification AFTER INSERT ON classification_events
WHEN EXISTS(SELECT 1 FROM effective_classification_events WHERE event_id = NEW.event_id)
BEGIN
 UPDATE _usage_report_operations SET phase=NEW.phase,activity=NEW.activity,activity_state=NEW.activity_state,attribution_provenance=NEW.provenance WHERE operation_id=NEW.operation_id;
END;
"#;
