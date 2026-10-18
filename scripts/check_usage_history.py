#!/usr/bin/env python3
"""Verify large synthetic usage histories before expensive release validation."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import time

from check_root_usage import Server


MARKER = (
    "Disposable isolated accounting verification copy. Never a production collector.\n"
)
FIXTURE_SHAPE_VERSION = 2


def expected_shape(requests):
    """Return deterministic lifetime-shape counts for the production fixture."""
    if type(requests) is not int or requests <= 0:
        raise ValueError("requests must be a positive integer")
    return {
        "open_operations": (requests + 510) // 511,
        "wait_spans": (requests + 15) // 16,
        "thread_count": 64,
        "phase_activity_pairs": 4,
        "token_facts": requests * 6 + 1,
    }


def native_probe(root, state, fixture, environment, test):
    return subprocess.run(
        [
            "just",
            "test",
            "--locked",
            "-p",
            "codex-usage",
            "--lib",
            "--run-ignored",
            "only",
            "--success-output",
            "final",
            "--retries",
            "0",
            "--profile",
            "usage-release",
            "--config-file",
            str(state / "nextest.toml"),
            "--no-tests=fail",
            "-E",
            f"test({test})",
        ],
        cwd=root,
        env=dict(environment, CXM_USAGE_SCALE_FIXTURE=str(fixture)),
        check=False,
    ).returncode


def scopes_for(fixture):
    if (fixture / "fixture-marker").read_text() != MARKER:
        raise ValueError("Unexpected usage fixture provenance")
    scopes = json.loads((fixture / "scope.json").read_text())
    if scopes.get("fixture_shape_version") != FIXTURE_SHAPE_VERSION:
        raise ValueError("Unsupported or legacy usage fixture shape")
    shape = expected_shape(scopes["expected_all_operations"])
    for key, expected in shape.items():
        if scopes.get(f"expected_{key}") != expected:
            raise ValueError(f"Invalid fixture shape count for {key}")
    rows = scopes["repositories"]
    if len(rows) != 8 or [row["name"] for row in rows] != [
        f"synthetic-{n}" for n in range(8)
    ]:
        raise ValueError("Expected eight generated repository windows")
    if any(
        row["expected_operations"] <= 0 or row["expected_provider_tokens"] <= 0
        for row in rows
    ):
        raise ValueError("Usage windows must carry independently calculated totals")
    return scopes


def copy_warming(ready, warming):
    scopes = scopes_for(ready)
    warming.mkdir(mode=0o700)
    (warming / "usage").mkdir(mode=0o700)
    with sqlite3.connect(
        f"file:{ready}/usage/usage.sqlite3?mode=ro", uri=True
    ) as source:
        source.execute("BEGIN")
        count, synthetic = source.execute(
            "SELECT COUNT(*), SUM(id LIKE '10000000-0000-4000-8000-%') FROM operations"
        ).fetchone()
        observations = source.execute(
            "SELECT COUNT(*) FROM token_observations"
        ).fetchone()[0]
        if count < 1_300_000 or synthetic != count or observations < 7_800_000:
            raise ValueError("Missing production-shaped synthetic history")
        open_operations = source.execute(
            "SELECT COUNT(*) FROM operations AS operation WHERE NOT EXISTS "
            "(SELECT 1 FROM operation_events AS event "
            "WHERE event.operation_id = operation.id AND event.terminal = 1)"
        ).fetchone()[0]
        wait_spans = source.execute("SELECT COUNT(*) FROM activity_spans").fetchone()[0]
        thread_count = source.execute("SELECT COUNT(*) FROM threads").fetchone()[0]
        phase_activity_pairs = source.execute(
            "SELECT COUNT(DISTINCT phase || ':' || activity) FROM operations"
        ).fetchone()[0]
        if (
            open_operations != scopes["expected_open_operations"]
            or wait_spans != scopes["expected_wait_spans"]
            or thread_count != scopes["expected_thread_count"]
            or phase_activity_pairs != scopes["expected_phase_activity_pairs"]
            or observations != scopes["expected_token_facts"]
        ):
            raise ValueError(
                "Generated fixture does not have the required lifetime shape"
            )
        if source.execute("SELECT ready FROM _usage_report_cache_meta").fetchall() != [
            (1,)
        ]:
            raise ValueError("Generated ready fixture is incomplete")
        size = (
            source.execute("PRAGMA page_count").fetchone()[0]
            * source.execute("PRAGMA page_size").fetchone()[0]
        )
        if shutil.disk_usage(warming.parent).free < size + 2 * 1024**3:
            raise ValueError("Insufficient disk storage for the warming fixture")
        with sqlite3.connect(warming / "usage/usage.sqlite3") as destination:
            source.backup(destination, pages=4096, sleep=0.1)
        source.rollback()
    # The existing scale fixture uses this same derived-metadata reset. Opening
    # the disposable copy runs the source-owned cache preparation and backfill.
    with sqlite3.connect(warming / "usage/usage.sqlite3") as destination:
        destination.execute("DELETE FROM _usage_report_cache_meta")
    for name in ("fixture-marker", "scope.json", "usage/repository-hmac.key"):
        shutil.copy2(ready / name, warming / name)


def verify_native(fixture, scopes):
    proof = json.loads((fixture / "native-window-proof.json").read_text())
    expected = {row["name"]: row for row in scopes["repositories"]}
    if (
        not isinstance(proof, list)
        or len(proof) != len(expected)
        or any(
            not isinstance(row, dict) or not isinstance(row.get("repository"), str)
            for row in proof
        )
        or {row["repository"] for row in proof} != set(expected)
    ):
        return False
    return all(
        all(
            type(row.get(key)) is int and 0 <= row[key] < 2**64
            for key in ("operations", "provider_tokens", "elapsed_ms")
        )
        and row["operations"] == expected[row["repository"]]["expected_operations"]
        and row["provider_tokens"]
        == expected[row["repository"]]["expected_provider_tokens"]
        and row["elapsed_ms"] <= 15_000
        for row in proof
    )


def public_probe(
    binary,
    fixture,
    scopes,
    environment,
    *,
    scope="windows",
    evidence_name="public-window-proof.json",
):
    reports = []
    if scope in {"all", "warming"}:
        cases = [
            {
                "name": "all",
                "params": {},
                "expected_operations": scopes["expected_all_operations"],
                "expected_provider_tokens": scopes["expected_all_provider_tokens"],
            }
        ]
    elif scope == "windows":
        cases = [
            dict(
                repository,
                params={
                    "repositoryKey": repository["repository_id"],
                    "fromAt": scopes["window_start_ms"] // 1000,
                    "toAt": scopes["window_end_ms"] // 1000,
                },
            )
            for repository in scopes["repositories"]
        ]
    else:
        raise ValueError("Unknown report scope")
    with (
        (fixture / (evidence_name + ".stderr.log")).open("w") as log,
        Server(
            binary, dict(environment, CODEX_HOME=str(fixture)), fixture, stderr=log
        ) as server,
    ):
        for repository in cases:
            started = time.monotonic()
            row = {"repository": repository["name"], "passed": False}
            try:
                server.counter += 1
                server.send(
                    {
                        "id": server.counter,
                        "method": "localUsage/summary",
                        "params": repository["params"],
                    }
                )
                while True:
                    remaining = 30 - (time.monotonic() - started)
                    if remaining <= 0:
                        raise TimeoutError("Report did not complete")
                    message = server.receive(timeout=remaining)
                    if message.get("id") == server.counter:
                        break
                if "error" in message:
                    error = message["error"]
                    code = error.get("code")
                    row["error_code"] = (
                        code if type(code) is int and -(2**31) <= code < 2**31 else None
                    )
                    failure = error.get("data") or {}
                    kind = failure.get("code") if isinstance(failure, dict) else None
                    if kind in {
                        "usage_report_busy",
                        "usage_report_timeout",
                        "usage_report_warming",
                        "usage_storage_unavailable",
                    }:
                        row["failure"] = kind
                    if scope == "warming":
                        cache = failure.get("cache") or {}
                        row["passed"] = (
                            kind == "usage_report_warming"
                            and isinstance(cache, dict)
                            and type(cache.get("schemaVersion")) is int
                            and cache["schemaVersion"] > 0
                            and cache.get("ready") is False
                            and isinstance(cache.get("progress"), list)
                            and bool(cache["progress"])
                            and all(
                                isinstance(progress, dict)
                                and isinstance(progress.get("source"), str)
                                and bool(progress["source"])
                                and type(progress.get("cursor")) is int
                                and type(progress.get("highWater")) is int
                                and 0
                                <= progress["cursor"]
                                <= progress["highWater"]
                                < 2**63
                                for progress in cache["progress"]
                            )
                            and failure.get("retryable") is True
                            and type(failure.get("retryAfterMs")) is int
                            and failure["retryAfterMs"] > 0
                            and time.monotonic() - started <= 2
                        )
                else:
                    result = message["result"]
                    operations = result["report"]["counts"]["operations"]
                    tokens = result["aggregate"]["totalTokens"]
                    if any(
                        type(value) is not int or not 0 <= value < 2**64
                        for value in (operations, tokens)
                    ):
                        raise ValueError("Invalid numeric report totals")
                    row.update(operations=operations, tokens=tokens)
                    row["passed"] = (
                        scope != "warming"
                        and row["operations"] == repository["expected_operations"]
                        and row["tokens"] == repository["expected_provider_tokens"]
                        and time.monotonic() - started <= 15
                    )
            except Exception as error:
                row["error_type"] = type(error).__name__
            row["elapsed_ms"] = round((time.monotonic() - started) * 1000)
            reports.append(row)
            if server.process.poll() is not None:
                break
    (fixture / evidence_name).write_text(json.dumps(reports, indent=2) + "\n")
    return len(reports) == len(cases) and all(row["passed"] for row in reports)


def check(root, state, binary):
    os.umask(0o077)
    state.mkdir(mode=0o700)
    if shutil.disk_usage(state).free < 32 * 1024**3:
        raise ValueError(
            "Large usage verification requires 32 GiB of free disk storage"
        )
    (state / "generation").mkdir(mode=0o700)
    (state / "nextest.toml").write_text(
        '[profile.default]\n[profile.usage-release]\ninherits = "default"\n'
        'slow-timeout = { period = "60s", terminate-after = 180 }\nretries = 0\n'
    )
    environment = dict(os.environ, CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG="1")
    for name in ("OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"):
        environment.pop(name, None)
    # An earlier test build may only have produced a hashed test executable.
    # Always bind the public probe to this checkout's actual CLI before use.
    subprocess.run(
        ["cargo", "build", "--locked", "-p", "codex-cli", "--bin", "codex"],
        cwd=root / "codex-rs",
        env=environment,
        check=True,
    )
    ready, warming = state / "ready", state / "warming"
    scale_environment = dict(
        environment,
        CXM_USAGE_SCALE_REQUESTS="1300000",
        CXM_USAGE_SCALE_ROOT=str(state / "generation"),
        CXM_USAGE_SCALE_RETAIN=str(ready),
    )
    scale_status = native_probe(
        root,
        state,
        ready,
        scale_environment,
        "accounting_scale_bounds_memory_refresh_and_wal",
    )
    copy_warming(ready, warming)
    # Initialize before pinning the snapshot, then keep refresh incomplete for
    # both public and native windows rather than accidentally testing warm data.
    subprocess.run(
        [str(binary), "usage", "repositories", "--limit", "1", "--json"],
        env=dict(environment, CODEX_HOME=str(warming)),
        check=True,
        stdout=subprocess.DEVNULL,
        timeout=180,
    )
    results = {"scale": scale_status == 0}
    with sqlite3.connect(
        f"file:{warming}/usage/usage.sqlite3?mode=ro", uri=True
    ) as reader:
        reader.execute("BEGIN")
        reader.execute("SELECT id FROM operations LIMIT 1").fetchone()
        for label, fixture in (("ready", ready), ("warming", warming)):
            scopes = scopes_for(fixture)
            results[f"{label}_public"] = public_probe(
                binary, fixture, scopes, environment
            )
            if label == "ready":
                results["ready_public_all"] = public_probe(
                    binary,
                    fixture,
                    scopes,
                    environment,
                    scope="all",
                    evidence_name="public-global-proof.json",
                )
            else:
                results["warming_public_all"] = public_probe(
                    binary,
                    fixture,
                    scopes,
                    environment,
                    scope="warming",
                    evidence_name="public-warming-proof.json",
                )
            native_status = native_probe(
                root, state, fixture, environment, "isolated_daily_window_scale_probe"
            )
            try:
                results[f"{label}_native"] = native_status == 0 and verify_native(
                    fixture, scopes
                )
            except (OSError, ValueError, KeyError):
                results[f"{label}_native"] = False
        reader.rollback()
    with sqlite3.connect(
        f"file:{warming}/usage/usage.sqlite3?mode=ro", uri=True
    ) as connection:
        results["rebuilding_exercised"] = connection.execute(
            "SELECT ready FROM _usage_report_cache_meta"
        ).fetchall() == [(0,)]
    with binary.open("rb") as executable:
        checksum = hashlib.file_digest(executable, "sha256").hexdigest()
    report = {"checks": results, "binary_sha256": checksum, "live_data_used": False}
    (state / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report), flush=True)
    return 0 if all(results.values()) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    binary = Path(os.environ["CARGO_TARGET_DIR"]) / "debug/codex"
    return check(root, args.state.resolve(), binary.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
