//! Physical schema for the versioned global report component.

pub(crate) const COMPONENT_VERSION: i64 = 3;

pub(crate) const RESET_SQL: &str = r#"
DROP TRIGGER IF EXISTS _usage_report_global_operation_insert;
DROP TRIGGER IF EXISTS _usage_report_global_operation_terminal;
DROP TRIGGER IF EXISTS _usage_report_global_token;
DROP TRIGGER IF EXISTS _usage_report_global_coverage;
DROP TRIGGER IF EXISTS _usage_report_global_classification;
DROP TRIGGER IF EXISTS _usage_report_global_gap;
DROP TRIGGER IF EXISTS _usage_report_global_span_insert;
DROP TRIGGER IF EXISTS _usage_report_global_span_end;
DROP TABLE IF EXISTS _usage_report_global_gap_events;
DROP TABLE IF EXISTS _usage_report_global_gap_v9;
DROP TABLE IF EXISTS _usage_report_global_tools;
DROP TABLE IF EXISTS _usage_report_global_time;
DROP TABLE IF EXISTS _usage_report_global_request_hours;
DROP TABLE IF EXISTS _usage_report_global_request;
DROP TABLE IF EXISTS _usage_report_global_unpriced;
DROP TABLE IF EXISTS _usage_report_global_unresolved;
DROP TABLE IF EXISTS _usage_report_global_dirty_hours;
DROP TABLE IF EXISTS _usage_report_global_dirty_hour_ranges;
DROP TABLE IF EXISTS _usage_report_global_dirty_operations;
DROP TABLE IF EXISTS _usage_report_global_classifications;
DROP TABLE IF EXISTS _usage_report_global_counts;
DROP TABLE IF EXISTS _usage_report_global_operations;
DROP TABLE IF EXISTS _usage_report_global_unpriced_meta;
DROP TABLE IF EXISTS _usage_report_global_unresolved_meta;
DROP TABLE IF EXISTS _usage_report_global_meta;
DROP TABLE IF EXISTS _usage_report_global_seed;
"#;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE _usage_report_global_meta(
 singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1),
 component_version INTEGER NOT NULL,
 core_schema_version INTEGER NOT NULL,
 core_generation INTEGER NOT NULL,
 ready INTEGER NOT NULL CHECK(ready IN (0,1)),
 dirty_revision INTEGER NOT NULL,
 applied_revision INTEGER NOT NULL
) STRICT;
CREATE TABLE _usage_report_global_seed(
 source TEXT PRIMARY KEY NOT NULL,
 cursor INTEGER NOT NULL,
 high_water INTEGER NOT NULL CHECK(high_water>=cursor)
) STRICT;
CREATE TABLE _usage_report_global_operations(
 operation_id TEXT PRIMARY KEY NOT NULL,
 operation_kind TEXT NOT NULL,
 terminal_status TEXT,
 started_at_ms INTEGER NOT NULL,
 ended_at_ms INTEGER,
 phase TEXT NOT NULL,
 activity TEXT NOT NULL,
 provenance TEXT NOT NULL
) STRICT;
CREATE TABLE _usage_report_global_counts(
 singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1),
 operations INTEGER NOT NULL,
 model_requests INTEGER NOT NULL,
 unfinished_operations INTEGER NOT NULL
) STRICT;
CREATE TABLE _usage_report_global_classifications(
 phase TEXT NOT NULL,
 activity TEXT NOT NULL,
 provenance TEXT NOT NULL,
 operation_count INTEGER NOT NULL,
 PRIMARY KEY(phase,activity,provenance)
) STRICT;
CREATE TABLE _usage_report_global_dirty_operations(operation_id TEXT PRIMARY KEY NOT NULL) STRICT;
CREATE TABLE _usage_report_global_dirty_hours(hour_index INTEGER PRIMARY KEY NOT NULL) STRICT;
CREATE TABLE _usage_report_global_dirty_hour_ranges(start_hour INTEGER PRIMARY KEY NOT NULL, end_hour INTEGER NOT NULL CHECK(end_hour>=start_hour)) STRICT;
CREATE TABLE _usage_report_global_unresolved(
 operation_id TEXT NOT NULL,
 scope_kind TEXT NOT NULL,
 unresolved INTEGER NOT NULL CHECK(unresolved IN(0,1)),
 PRIMARY KEY(operation_id,scope_kind)
) STRICT;
CREATE TABLE _usage_report_global_unpriced(operation_id TEXT PRIMARY KEY NOT NULL) STRICT;
CREATE TABLE _usage_report_global_unresolved_meta(singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1), unresolved_count INTEGER NOT NULL) STRICT;
CREATE TABLE _usage_report_global_unpriced_meta(singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1), unpriced_count INTEGER NOT NULL) STRICT;
CREATE TABLE _usage_report_global_gap_v9(singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1), has_gap INTEGER NOT NULL CHECK(has_gap IN(0,1))) STRICT;
CREATE TABLE _usage_report_global_gap_events(
 event_id TEXT PRIMARY KEY NOT NULL,
 occurred_at_ms INTEGER NOT NULL,
 coverage_state TEXT NOT NULL
) STRICT;
CREATE INDEX _usage_report_global_gap_events_latest_idx ON _usage_report_global_gap_events(occurred_at_ms DESC,event_id DESC);
CREATE TABLE _usage_report_global_time(
 hour_index INTEGER NOT NULL,
 kind TEXT NOT NULL,
 label TEXT NOT NULL,
 measured_ms INTEGER NOT NULL,
 unknown_intervals INTEGER NOT NULL,
 zero_intervals INTEGER NOT NULL,
 interval_count INTEGER NOT NULL,
 PRIMARY KEY(hour_index,kind,label)
) STRICT;
CREATE TABLE _usage_report_global_tools(
 hour_index INTEGER NOT NULL,
 outcome TEXT NOT NULL,
 measured_ms INTEGER NOT NULL,
 unknown_intervals INTEGER NOT NULL,
 operation_count INTEGER NOT NULL,
 PRIMARY KEY(hour_index,outcome)
) STRICT;
CREATE TABLE _usage_report_global_request(singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton=1), min_start_ms INTEGER, max_end_ms INTEGER, unknown_intervals INTEGER NOT NULL, request_count INTEGER NOT NULL) STRICT;
CREATE TABLE _usage_report_global_request_hours(
 hour_index INTEGER PRIMARY KEY NOT NULL,
 min_start_ms INTEGER,
 max_end_ms INTEGER,
 unknown_intervals INTEGER NOT NULL,
 request_count INTEGER NOT NULL
) STRICT;
INSERT INTO _usage_report_global_counts VALUES(1,0,0,0);
INSERT INTO _usage_report_global_unresolved_meta VALUES(1,0);
INSERT INTO _usage_report_global_unpriced_meta VALUES(1,0);
INSERT INTO _usage_report_global_gap_v9 VALUES(1,0);
INSERT INTO _usage_report_global_request VALUES(1,NULL,NULL,0,0);
INSERT INTO _usage_report_global_meta VALUES(1,1,0,0,0,0,0);
INSERT INTO _usage_report_global_seed VALUES
 ('operations',0,0),
 ('token_observations',0,0),
 ('coverage_events',0,0),
 ('activity_spans',0,0);
CREATE TRIGGER _usage_report_global_operation_insert AFTER INSERT ON operations BEGIN
 INSERT OR IGNORE INTO _usage_report_global_dirty_operations VALUES(NEW.id);
 INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) VALUES(NEW.started_at_ms/3600000-(NEW.started_at_ms%3600000<0),NEW.started_at_ms/3600000-(NEW.started_at_ms%3600000<0)) ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour);
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_operation_terminal AFTER INSERT ON operation_events WHEN NEW.terminal=1 BEGIN
 INSERT OR IGNORE INTO _usage_report_global_dirty_operations VALUES(NEW.operation_id);
 INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT started_at_ms/3600000-(started_at_ms%3600000<0),MAX(started_at_ms/3600000-(started_at_ms%3600000<0),NEW.occurred_at_ms/3600000-(NEW.occurred_at_ms%3600000<0)) FROM operations WHERE id=NEW.operation_id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour);
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_token AFTER INSERT ON token_observations BEGIN
 INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT operation_id FROM model_requests WHERE id=NEW.model_request_id;
 INSERT OR IGNORE INTO _usage_report_global_dirty_operations SELECT operation_id FROM tool_invocations WHERE id=NEW.tool_invocation_id;
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_coverage AFTER INSERT ON coverage_events WHEN NEW.operation_id IS NOT NULL BEGIN
 INSERT OR IGNORE INTO _usage_report_global_dirty_operations VALUES(NEW.operation_id);
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_gap AFTER INSERT ON coverage_events WHEN NEW.operation_id IS NULL BEGIN
 INSERT OR IGNORE INTO _usage_report_global_gap_events(event_id,occurred_at_ms,coverage_state) VALUES(NEW.event_id,NEW.occurred_at_ms,NEW.coverage_state);
 UPDATE _usage_report_global_gap_v9 SET has_gap=COALESCE((SELECT coverage_state<>'complete' FROM _usage_report_global_gap_events ORDER BY occurred_at_ms DESC,event_id DESC LIMIT 1),0) WHERE singleton=1;
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_classification AFTER INSERT ON classification_events BEGIN
 INSERT OR IGNORE INTO _usage_report_global_dirty_operations VALUES(NEW.operation_id);
 INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0),MAX(operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0),COALESCE(terminal.occurred_at_ms/3600000-(terminal.occurred_at_ms%3600000<0),operation.started_at_ms/3600000-(operation.started_at_ms%3600000<0))) FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 WHERE operation.id=NEW.operation_id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour);
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_span_insert AFTER INSERT ON activity_spans BEGIN
 INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) VALUES(NEW.started_at_ms/3600000-(NEW.started_at_ms%3600000<0),NEW.started_at_ms/3600000-(NEW.started_at_ms%3600000<0)) ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour);
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
CREATE TRIGGER _usage_report_global_span_end AFTER INSERT ON activity_span_events WHEN NEW.event_kind='ended' BEGIN
 INSERT INTO _usage_report_global_dirty_hour_ranges(start_hour,end_hour) SELECT started_at_ms/3600000-(started_at_ms%3600000<0),MAX(started_at_ms/3600000-(started_at_ms%3600000<0),NEW.occurred_at_ms/3600000-(NEW.occurred_at_ms%3600000<0)) FROM activity_spans WHERE id=NEW.activity_span_id ON CONFLICT(start_hour) DO UPDATE SET end_hour=MAX(end_hour,excluded.end_hour);
 UPDATE _usage_report_global_meta SET dirty_revision=dirty_revision+1 WHERE singleton=1;
END;
"#;
