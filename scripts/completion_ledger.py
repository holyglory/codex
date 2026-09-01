#!/usr/bin/env python3
"""Reviewed CLI for the CodexMulti append-only completion ledger."""

import argparse
import json
import os
import sqlite3
import stat
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
DEFAULT_DB = (
    Path(__file__).resolve().parents[1] / ".state" / "completion-ledger.sqlite3"
)
UNRESOLVED_STATUSES = ("active", "implemented", "blocked")
ALL_STATUSES = (*UNRESOLVED_STATUSES, "verified", "superseded", "released")

SCHEMA = """
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS issues (
    id TEXT PRIMARY KEY,
    requirement_key TEXT NOT NULL,
    title TEXT NOT NULL,
    remaining_outcome TEXT NOT NULL,
    impact TEXT NOT NULL,
    current_state TEXT NOT NULL,
    unblock_condition TEXT NOT NULL,
    verification TEXT NOT NULL,
    affected_paths_json TEXT NOT NULL,
    priority TEXT NOT NULL CHECK (priority IN ('P0', 'P1', 'P2', 'P3')),
    owner TEXT,
    status TEXT NOT NULL CHECK (
        status IN ('active', 'implemented', 'blocked', 'verified', 'superseded', 'released')
    ),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS issue_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    issue_id TEXT NOT NULL REFERENCES issues(id),
    event_type TEXT NOT NULL,
    from_status TEXT,
    to_status TEXT,
    note TEXT NOT NULL,
    verification_evidence TEXT,
    superseded_by TEXT,
    event_data_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS issues_status_priority_idx
    ON issues(status, priority, created_at);
CREATE INDEX IF NOT EXISTS issue_events_issue_sequence_idx
    ON issue_events(issue_id, sequence);

CREATE TRIGGER IF NOT EXISTS issue_events_no_update
BEFORE UPDATE ON issue_events
BEGIN
    SELECT RAISE(ABORT, 'completion ledger events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS issue_events_no_delete
BEFORE DELETE ON issue_events
BEGIN
    SELECT RAISE(ABORT, 'completion ledger events cannot be deleted');
END;

CREATE TRIGGER IF NOT EXISTS issues_no_delete
BEFORE DELETE ON issues
BEGIN
    SELECT RAISE(ABORT, 'completion ledger issues cannot be deleted');
END;
"""


class LedgerError(Exception):
    """A user-actionable completion-ledger error."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def json_text(value: Any) -> str:
    return json.dumps(value, separators=(",", ":"), sort_keys=True)


def ensure_mode(path: Path, preferred_mode: int, private_fallback_mode: int) -> None:
    try:
        os.chmod(path, preferred_mode)
    except PermissionError:
        if stat.S_IMODE(path.stat().st_mode) not in {
            preferred_mode,
            private_fallback_mode,
        }:
            raise


def open_ledger(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    ensure_mode(path.parent, 0o2770, 0o700)
    connection = sqlite3.connect(path, timeout=5.0)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA foreign_keys = ON")
    connection.execute("PRAGMA busy_timeout = 5000")
    connection.execute("PRAGMA journal_mode = WAL")
    connection.execute("PRAGMA synchronous = FULL")
    connection.executescript(SCHEMA)
    connection.execute(
        "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (?, ?)",
        (SCHEMA_VERSION, utc_now()),
    )
    connection.commit()
    ensure_mode(path, 0o660, 0o600)
    return connection


def row_dict(row: sqlite3.Row) -> dict[str, Any]:
    result = dict(row)
    if "affected_paths_json" in result:
        result["affected_paths"] = json.loads(result.pop("affected_paths_json"))
    if "event_data_json" in result:
        result["event_data"] = json.loads(result.pop("event_data_json"))
    return result


def require_issue(connection: sqlite3.Connection, issue_id: str) -> sqlite3.Row:
    row = connection.execute(
        "SELECT * FROM issues WHERE id = ?", (issue_id,)
    ).fetchone()
    if row is None:
        raise LedgerError(f"unknown completion-ledger issue: {issue_id}")
    return row


def append_event(
    connection: sqlite3.Connection,
    *,
    issue_id: str,
    event_type: str,
    note: str,
    event_data: dict[str, Any],
    from_status: str | None = None,
    to_status: str | None = None,
    verification_evidence: str | None = None,
    superseded_by: str | None = None,
) -> None:
    connection.execute(
        """
        INSERT INTO issue_events(
            event_id, issue_id, event_type, from_status, to_status, note,
            verification_evidence, superseded_by, event_data_json, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            f"evt-{uuid.uuid4().hex}",
            issue_id,
            event_type,
            from_status,
            to_status,
            note,
            verification_evidence,
            superseded_by,
            json_text(event_data),
            utc_now(),
        ),
    )


def command_init(
    connection: sqlite3.Connection, _args: argparse.Namespace
) -> dict[str, Any]:
    integrity = connection.execute("PRAGMA integrity_check").fetchone()[0]
    return {"schemaVersion": SCHEMA_VERSION, "integrity": integrity}


def command_add(
    connection: sqlite3.Connection, args: argparse.Namespace
) -> dict[str, Any]:
    issue_id = f"CML-{uuid.uuid4().hex[:12].upper()}"
    now = utc_now()
    issue = {
        "id": issue_id,
        "requirement_key": args.requirement,
        "title": args.title,
        "remaining_outcome": args.outcome,
        "impact": args.impact,
        "current_state": args.current_state,
        "unblock_condition": args.unblock_condition,
        "verification": args.verification,
        "affected_paths": args.path,
        "priority": args.priority,
        "owner": args.owner,
        "status": "active",
        "created_at": now,
        "updated_at": now,
    }
    with connection:
        connection.execute(
            """
            INSERT INTO issues(
                id, requirement_key, title, remaining_outcome, impact,
                current_state, unblock_condition, verification,
                affected_paths_json, priority, owner, status, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                issue_id,
                args.requirement,
                args.title,
                args.outcome,
                args.impact,
                args.current_state,
                args.unblock_condition,
                args.verification,
                json_text(args.path),
                args.priority,
                args.owner,
                "active",
                now,
                now,
            ),
        )
        append_event(
            connection,
            issue_id=issue_id,
            event_type="created",
            note=args.current_state,
            event_data=issue,
            to_status="active",
        )
    return issue


def command_list(
    connection: sqlite3.Connection, args: argparse.Namespace
) -> dict[str, Any]:
    parameters: list[Any] = []
    where = ""
    if args.status == "unresolved":
        placeholders = ",".join("?" for _ in UNRESOLVED_STATUSES)
        where = f"WHERE status IN ({placeholders})"
        parameters.extend(UNRESOLVED_STATUSES)
    elif args.status != "all":
        where = "WHERE status = ?"
        parameters.append(args.status)
    parameters.append(args.limit)
    rows = connection.execute(
        f"""
        SELECT * FROM issues
        {where}
        ORDER BY priority, created_at, id
        LIMIT ?
        """,
        parameters,
    ).fetchall()
    return {"schemaVersion": SCHEMA_VERSION, "issues": [row_dict(row) for row in rows]}


def command_show(
    connection: sqlite3.Connection, args: argparse.Namespace
) -> dict[str, Any]:
    issue = row_dict(require_issue(connection, args.issue_id))
    events = connection.execute(
        "SELECT * FROM issue_events WHERE issue_id = ? ORDER BY sequence LIMIT ?",
        (args.issue_id, args.limit),
    ).fetchall()
    return {
        "schemaVersion": SCHEMA_VERSION,
        "issue": issue,
        "events": [row_dict(row) for row in events],
    }


def command_transition(
    connection: sqlite3.Connection, args: argparse.Namespace
) -> dict[str, Any]:
    issue = require_issue(connection, args.issue_id)
    from_status = issue["status"]
    if args.status in ("verified", "released") and not args.verification_evidence:
        raise LedgerError(f"{args.status} requires --verification-evidence")
    if args.status == "blocked" and not args.unblock_condition:
        raise LedgerError("blocked requires --unblock-condition")
    if args.status == "superseded" and not args.superseded_by:
        raise LedgerError("superseded requires --superseded-by")

    current_state = args.current_state or issue["current_state"]
    unblock_condition = args.unblock_condition or issue["unblock_condition"]
    verification = args.verification or issue["verification"]
    owner = args.owner if args.owner is not None else issue["owner"]
    now = utc_now()
    event_data = {
        "current_state": current_state,
        "unblock_condition": unblock_condition,
        "verification": verification,
        "owner": owner,
    }
    with connection:
        append_event(
            connection,
            issue_id=args.issue_id,
            event_type="transition",
            note=args.note,
            event_data=event_data,
            from_status=from_status,
            to_status=args.status,
            verification_evidence=args.verification_evidence,
            superseded_by=args.superseded_by,
        )
        connection.execute(
            """
            UPDATE issues
            SET status = ?, current_state = ?, unblock_condition = ?,
                verification = ?, owner = ?, updated_at = ?
            WHERE id = ?
            """,
            (
                args.status,
                current_state,
                unblock_condition,
                verification,
                owner,
                now,
                args.issue_id,
            ),
        )
    return row_dict(require_issue(connection, args.issue_id))


def command_note(
    connection: sqlite3.Connection, args: argparse.Namespace
) -> dict[str, Any]:
    issue = require_issue(connection, args.issue_id)
    current_state = args.current_state or issue["current_state"]
    unblock_condition = args.unblock_condition or issue["unblock_condition"]
    verification = args.verification or issue["verification"]
    owner = args.owner if args.owner is not None else issue["owner"]
    now = utc_now()
    event_data = {
        "current_state": current_state,
        "unblock_condition": unblock_condition,
        "verification": verification,
        "owner": owner,
    }
    with connection:
        append_event(
            connection,
            issue_id=args.issue_id,
            event_type="note",
            note=args.note,
            event_data=event_data,
            from_status=issue["status"],
            to_status=issue["status"],
        )
        connection.execute(
            """
            UPDATE issues
            SET current_state = ?, unblock_condition = ?, verification = ?,
                owner = ?, updated_at = ?
            WHERE id = ?
            """,
            (
                current_state,
                unblock_condition,
                verification,
                owner,
                now,
                args.issue_id,
            ),
        )
    return row_dict(require_issue(connection, args.issue_id))


def command_doctor(
    connection: sqlite3.Connection, _args: argparse.Namespace
) -> dict[str, Any]:
    integrity = connection.execute("PRAGMA integrity_check").fetchone()[0]
    versions = [
        row[0]
        for row in connection.execute(
            "SELECT version FROM schema_migrations ORDER BY version"
        ).fetchall()
    ]
    unresolved = connection.execute(
        "SELECT COUNT(*) FROM issues WHERE status IN ('active', 'implemented', 'blocked')"
    ).fetchone()[0]
    return {
        "schemaVersion": SCHEMA_VERSION,
        "integrity": integrity,
        "migrations": versions,
        "unresolved": unresolved,
    }


def add_projection_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--current-state")
    parser.add_argument("--unblock-condition")
    parser.add_argument("--verification")
    parser.add_argument("--owner")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--db", type=Path, default=DEFAULT_DB)
    subparsers = result.add_subparsers(dest="command", required=True)

    init_parser = subparsers.add_parser("init")
    init_parser.set_defaults(handler=command_init)

    add_parser = subparsers.add_parser("add")
    add_parser.add_argument("--requirement", required=True)
    add_parser.add_argument("--title", required=True)
    add_parser.add_argument("--outcome", required=True)
    add_parser.add_argument("--impact", required=True)
    add_parser.add_argument("--current-state", required=True)
    add_parser.add_argument("--unblock-condition", required=True)
    add_parser.add_argument("--verification", required=True)
    add_parser.add_argument("--path", action="append", default=[])
    add_parser.add_argument(
        "--priority", choices=("P0", "P1", "P2", "P3"), default="P1"
    )
    add_parser.add_argument("--owner")
    add_parser.set_defaults(handler=command_add)

    list_parser = subparsers.add_parser("list")
    list_parser.add_argument(
        "--status",
        choices=("unresolved", "all", *ALL_STATUSES),
        default="unresolved",
    )
    list_parser.add_argument("--limit", type=int, choices=range(1, 501), default=200)
    list_parser.set_defaults(handler=command_list)

    show_parser = subparsers.add_parser("show")
    show_parser.add_argument("issue_id")
    show_parser.add_argument("--limit", type=int, choices=range(1, 1001), default=500)
    show_parser.set_defaults(handler=command_show)

    transition_parser = subparsers.add_parser("transition")
    transition_parser.add_argument("issue_id")
    transition_parser.add_argument("--status", choices=ALL_STATUSES, required=True)
    transition_parser.add_argument("--note", required=True)
    transition_parser.add_argument("--verification-evidence")
    transition_parser.add_argument("--superseded-by")
    add_projection_arguments(transition_parser)
    transition_parser.set_defaults(handler=command_transition)

    note_parser = subparsers.add_parser("note")
    note_parser.add_argument("issue_id")
    note_parser.add_argument("--note", required=True)
    add_projection_arguments(note_parser)
    note_parser.set_defaults(handler=command_note)

    doctor_parser = subparsers.add_parser("doctor")
    doctor_parser.set_defaults(handler=command_doctor)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        connection = open_ledger(args.db.resolve())
        try:
            with connection:
                result = args.handler(connection, args)
        finally:
            connection.close()
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0
    except (LedgerError, sqlite3.Error, OSError) as error:
        print(json.dumps({"error": str(error)}, sort_keys=True), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
