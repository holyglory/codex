//! Current coverage projections avoid repeatedly scanning historical markers.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE _usage_report_latest_coverage(operation_id TEXT NOT NULL,scope_kind TEXT NOT NULL,event_id TEXT NOT NULL,occurred_at_ms INTEGER NOT NULL,coverage_state TEXT NOT NULL,reason_code TEXT,PRIMARY KEY(operation_id,scope_kind)) STRICT;
CREATE TABLE _usage_report_provider_complete(operation_id TEXT PRIMARY KEY NOT NULL) STRICT;
CREATE TABLE _usage_report_global_gap(singleton INTEGER PRIMARY KEY NOT NULL,has_gap INTEGER NOT NULL) STRICT;
INSERT INTO _usage_report_global_gap VALUES(1,0);
CREATE TRIGGER _usage_report_latest_coverage_insert AFTER INSERT ON coverage_events WHEN NEW.operation_id IS NOT NULL BEGIN
INSERT INTO _usage_report_latest_coverage(operation_id,scope_kind,event_id,occurred_at_ms,coverage_state,reason_code)
SELECT NEW.operation_id,NEW.scope_kind,NEW.event_id,NEW.occurred_at_ms,NEW.coverage_state,NEW.reason_code
WHERE NEW.operation_id IS NOT NULL
ON CONFLICT(operation_id,scope_kind) DO UPDATE SET event_id=excluded.event_id,occurred_at_ms=excluded.occurred_at_ms,coverage_state=excluded.coverage_state,reason_code=excluded.reason_code
WHERE excluded.occurred_at_ms > _usage_report_latest_coverage.occurred_at_ms OR (excluded.occurred_at_ms = _usage_report_latest_coverage.occurred_at_ms AND excluded.event_id > _usage_report_latest_coverage.event_id);
END;
CREATE TRIGGER _usage_report_global_gap_insert AFTER INSERT ON coverage_events WHEN NEW.operation_id IS NULL BEGIN
UPDATE _usage_report_global_gap SET has_gap = MAX(has_gap, NEW.coverage_state <> 'complete');
END;
CREATE TRIGGER _usage_report_provider_complete_insert AFTER INSERT ON token_observations WHEN NEW.model_request_id IS NOT NULL BEGIN
INSERT OR IGNORE INTO _usage_report_provider_complete(operation_id)
SELECT request.operation_id FROM model_requests AS request
WHERE request.id = NEW.model_request_id AND NEW.token_count IS NOT NULL AND NEW.coverage_state = 'complete'
AND NEW.measurement_provenance = 'provider_reported' AND NEW.category_path NOT GLOB 'attribution.items.*';
END;
"#;

pub(super) const COVERAGE: &str = r#"
INSERT INTO _usage_report_latest_coverage(operation_id,scope_kind,event_id,occurred_at_ms,coverage_state,reason_code)
SELECT token.operation_id,token.scope_kind,token.event_id,token.occurred_at_ms,token.coverage_state,token.reason_code
FROM coverage_events AS token WHERE token.rowid > ?1 AND token.rowid <= ?2 AND token.operation_id IS NOT NULL
ON CONFLICT(operation_id,scope_kind) DO UPDATE SET event_id=excluded.event_id,occurred_at_ms=excluded.occurred_at_ms,coverage_state=excluded.coverage_state,reason_code=excluded.reason_code
WHERE excluded.occurred_at_ms > _usage_report_latest_coverage.occurred_at_ms OR (excluded.occurred_at_ms = _usage_report_latest_coverage.occurred_at_ms AND excluded.event_id > _usage_report_latest_coverage.event_id);
"#;

pub(super) const PROVIDER: &str = r#"
INSERT OR IGNORE INTO _usage_report_provider_complete(operation_id)
SELECT request.operation_id FROM token_observations AS token JOIN model_requests AS request ON request.id = token.model_request_id
WHERE token.rowid > ?1 AND token.rowid <= ?2 AND token.token_count IS NOT NULL AND token.coverage_state = 'complete'
AND token.measurement_provenance = 'provider_reported' AND token.category_path NOT GLOB 'attribution.items.*';
"#;

pub(super) const GLOBAL: &str = r#"
UPDATE _usage_report_global_gap SET has_gap = MAX(has_gap, (SELECT COALESCE(MAX(coverage_state <> 'complete'),0) FROM coverage_events WHERE rowid > ?1 AND rowid <= ?2 AND operation_id IS NULL));
"#;

