"""Exercise the deployment verifier through its actual local WebSocket boundary."""

import base64
import copy
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from local_deploy_owner import digest
from local_deploy_rehearsal import PRESERVED_FILES
from local_deploy_rpc import verify
import prepare_local_usage_package as usage_package


def cost_fixture(model_requests=None, priced=None, unknown=None):
    return {
        "status": "partial" if priced else "unavailable",
        "basis": "api_equivalent",
        "currency": "USD",
        "model_requests": model_requests,
        "priced_requests": None,
        "unknown_requests": None,
        "priced_observations": priced,
        "unknown_observations": unknown,
        "estimated_usd_micros": 0 if priced else None,
        "input_tokens": None,
        "cached_input_tokens": None,
        "uncached_input_tokens": None,
        "output_tokens": None,
        "reasoning_tokens": None,
        "unavailable_reasons": {
            "component_cost_breakdown_unavailable"
            if priced
            else "outcome_cost_unobserved": 1
        },
    }


class Peer:
    def __init__(
        self, home, mutate=None, model_tools=False, code_mode=False, pending_headlines=0
    ):
        self.home = home
        self.mutate = mutate or (lambda method, value: value)
        self.model_tools = model_tools
        self.code_mode = code_mode
        self.turn_count = 0
        self.binding_revision = 0
        self.pending_headlines = pending_headlines
        endpoint = home / "app-server-control/app-server-control.sock"
        endpoint.parent.mkdir()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(endpoint))
        self.listener.listen(1)
        self.listener.settimeout(5)
        self.methods = []
        self.requests = []
        self.error = None
        self.worker = threading.Thread(target=self.run)

    def __enter__(self):
        self.worker.start()
        return self

    def __exit__(self, *_args):
        self.worker.join(timeout=6)
        self.listener.close()
        if self.worker.is_alive():
            raise AssertionError("verification peer did not exit")
        if self.error:
            raise self.error

    def result(self, method, params):
        if method == "initialize":
            return {
                "userAgent": "fixture/0.166.0+multi.3",
                "codexHome": str(self.home),
                "multiAccount": {"supportsCreditUsage": True},
                "localUsageAccounting": {
                    "version": 1,
                    "supportedVersions": [1],
                    "methods": (
                        ["localUsage/summary", "localUsage/repositoryIdentity"]
                        if not self.model_tools
                        else [
                            "localUsage/bindingStatus",
                            "localUsage/bindingTransition",
                            "localUsage/repositoryIdentity",
                            "localUsage/summary",
                        ]
                    ),
                    "storage": "usage-v2",
                    "supportsWorkContextBindings": True,
                    "supportsWorkstreamOutcomes": True,
                    "supportsTokenHeadlines": True,
                    "snapshotRetentionMs": 5000,
                },
            }
        if method == "localUsage/repositoryIdentity":
            return {
                "contractVersion": 1,
                "source": "usage-v2",
                "repositoryId": "fixture-repository",
                "formatVersion": 3,
            }
        if method == "localUsage/bindingStatus" and self.model_tools:
            transition = None
            if self.binding_revision in (1, 2):
                suffix = "a" if self.binding_revision == 1 else "b"
                transition = {
                    "threadId": "fixture-thread",
                    "nativeProjectId": "fixture-project",
                    "workstreamId": f"fixture-workstream-{suffix}",
                    "outcomeId": f"fixture-outcome-{suffix}",
                    "effectiveCaptureSequence": self.binding_revision,
                    "capturedAtMs": 1,
                    "source": "agent_declared",
                    "authority": "agent_declared",
                }
            elif self.binding_revision == 3:
                transition = {
                    "threadId": "fixture-thread",
                    "nativeProjectId": None,
                    "workstreamId": None,
                    "outcomeId": None,
                    "effectiveCaptureSequence": 3,
                    "capturedAtMs": 1,
                    "source": "agent_declared",
                    "authority": "agent_declared",
                }
            return {
                "version": 1,
                "revision": self.binding_revision,
                "transition": transition,
            }
        if method == "localUsage/summary":
            result = {
                "contractVersion": 1,
                "source": "usage-v2",
                "generatedAt": 2000,
                "snapshot": {
                    "sourceWatermark": "usage-v2:fixture",
                    "generation": 3,
                    "watermark": 2,
                    "journalWatermark": 3,
                    "generatedAt": 1999,
                    "watermarkAtMs": 1000,
                    "updatedAtMs": 1000,
                    "freshness": "fresh",
                    "pendingEntries": 0,
                    "pendingCorrections": 0,
                    "refreshId": None,
                },
                "report": {
                    "schemaVersion": 1,
                    "kind": "usageSummary",
                    "databaseSchemaVersion": 3,
                    "taxonomyVersion": 1,
                    "scope": {"type": "all", "id": None},
                    "timeRange": None,
                    "coverage": {"state": "partial", "hasGaps": True},
                    "counts": {"operations": 1, "modelRequests": 1, "tools": 0},
                },
                "summary": {
                    "schemaVersion": 1,
                    "kind": "usageSummary",
                    "records": 2,
                    "inputTokens": 100,
                    "cachedInputTokens": 50,
                    "outputTokens": 20,
                    "reasoningOutputTokens": 5,
                    "counts": {"operations": 1, "modelRequests": 1, "tools": 0},
                },
                "workContexts": [],
                "operationBindings": [],
            }
            if not params.get("includeReview"):
                result["snapshot"].update(
                    sourceWatermark="usage-v2:headline:fixture-1:g3:w2",
                    journalWatermark=2,
                )
                rows = [
                    ("total_tokens", 999, 999, 0),
                    ("input_tokens", 100, 100, 0),
                    ("input_tokens_details.cached_tokens", 50, 50, 0),
                    ("output_tokens", 20, 20, 0),
                    ("output_tokens_details.reasoning_tokens", 5, 5, 0),
                    ("cache_write_input_tokens", 0, None, 2),
                ]
                result["report"].update(
                    projection="token_headline",
                    counts=None,
                    providerTokens=[
                        {
                            "category": category,
                            "repositoryBucket": "all",
                            "measurementProvenance": "provider_reported",
                            "measuredTokens": measured,
                            "exactTokens": exact,
                            "unknownObservations": unknown,
                            "observationCount": 2,
                        }
                        for category, measured, exact, unknown in rows
                    ],
                    providerTokensByActivity=None,
                    timing=None,
                    lifecycle=None,
                    contextSize=None,
                    cost=None,
                    workBindings=None,
                    coverage={
                        "state": "complete",
                        "hasGaps": False,
                        "providerTotals": "recorded",
                        **{
                            key: "deferred"
                            for key in (
                                "counts",
                                "cost",
                                "timing",
                                "lifecycle",
                                "contextSize",
                                "workBindings",
                                "activity",
                            )
                        },
                        "details": {
                            "basis": "published_snapshot",
                            "exactAtPublishedWatermark": True,
                            "tokenObservations": 2,
                            "providerTotalUnknowns": 0,
                            "cacheWriteUnknowns": 2,
                            "missingTokenObservations": 0,
                            "sourceSnapshotId": "fixture",
                            "sourceFreshness": "fresh",
                            "latestJournalWatermark": 2,
                            "retiredObservations": 0,
                            "rawAdmissionCutoffAtMs": None,
                            "unappliedRetentionRecords": 0,
                            "unappliedRetentionCorrections": 0,
                        },
                    },
                )
                result["summary"] = {
                    "projection": "token_headline",
                    "counts": None,
                    "inputTokens": 100,
                    "cachedInputTokens": 50,
                    "cacheWriteInputTokens": None,
                    "outputTokens": 20,
                    "reasoningOutputTokens": 5,
                    "providerTotalTokens": 999,
                    "tokenObservations": 2,
                }
                result.update(
                    {
                        key: None
                        for key in (
                            "review",
                            "observations",
                            "operations",
                            "nextRecordsCursor",
                            "nextOperationsCursor",
                            "contextSize",
                            "costProvenance",
                            "lifecycle",
                            "timing",
                        )
                    }
                )
                return result
            if params.get("includeReview"):
                effort = {
                    "activeAgentMs": {"measured": 1500, "exact": 1500, "unknown": 0},
                    "elapsedExecutionMs": {"measured": 900, "exact": 900, "unknown": 0},
                    "recordedWaitMs": {"measured": 0, "exact": None, "unknown": 0},
                    "cost": cost_fixture(),
                }
                result["report"]["cost"] = {
                    "estimatedApiEquivalentCostMicros": {
                        "measured": 0,
                        "exact": None,
                        "unknown": 1,
                    },
                    "actualBillingMicros": {
                        "measured": 11,
                        "exact": None,
                        "unknown": 1,
                    },
                }
                result["report"]["scope"] = {
                    "type": "repository",
                    "id": "fixture-repository",
                }
                result["report"]["timeRange"] = {
                    "startMs": params["fromAtMs"],
                    "endMs": params["toAtMs"],
                }
                result["report"]["workBindings"] = {
                    "schemaVersion": 1,
                    "basis": "request_declarations",
                    "declaredOperations": 1,
                }
                result["review"] = {
                    "totals": {
                        "model_requests": 1,
                        "cost": cost_fixture(model_requests=1, priced=1, unknown=1),
                    },
                    "outcomes": {
                        "schemaVersion": 1,
                        "coverage": "unattributed",
                        "rows": [],
                        "totals": effort,
                        "attributed": effort,
                        "unattributed": effort,
                    },
                }
            if self.model_tools and params.get("threadId") == "fixture-thread":

                def effort(operations, tokens):
                    return {
                        "operations": operations,
                        "providerTotalTokens": {
                            "measured": tokens,
                            "unknown": 0,
                            "exact": None,
                        },
                        "activeAgentMs": {
                            "measured": 0,
                            "unknown": operations,
                            "exact": None,
                        },
                        "elapsedExecutionMs": {
                            "measured": 0,
                            "unknown": operations,
                            "exact": None,
                        },
                        "recordedWaitMs": {"measured": 0, "unknown": 0, "exact": None},
                        "retryOperations": 0,
                        "reworkOperations": 0,
                        "cost": cost_fixture(),
                    }

                result["report"]["scope"] = {"type": "thread", "id": "fixture-thread"}
                result["report"]["timeRange"] = {
                    "startMs": params["fromAtMs"],
                    "endMs": params["toAtMs"],
                }
                result["report"]["workBindings"] = {
                    "schemaVersion": 1,
                    "basis": "explicit_sparse_transitions",
                    "boundOperations": 2,
                    "unknownOperations": 1,
                    "transitions": 3,
                }
                result["review"] = {
                    "outcomes": {
                        "schemaVersion": 1,
                        "coverage": "partial",
                        "totals": effort(3, 200),
                        "attributed": effort(2, 180),
                        "unattributed": effort(1, 20),
                        "rows": [
                            {
                                "outcomeId": "fixture-outcome-a",
                                "workstreamId": "fixture-workstream-a",
                                "nativeProjectId": "fixture-project",
                                "effort": effort(1, 100),
                            },
                            {
                                "outcomeId": "fixture-outcome-b",
                                "workstreamId": "fixture-workstream-b",
                                "nativeProjectId": "fixture-project",
                                "effort": effort(1, 80),
                            },
                        ],
                    }
                }
            return result
        if method == "thread/start" and self.model_tools:
            return {"thread": {"id": "fixture-thread"}}
        if method == "turn/start" and self.model_tools:
            return {"turn": {"id": "fixture-turn"}}
        if method in {"model/list", "accountProfile/list"}:
            return {"data": []}
        if method == "skills/list":
            return {"data": []}
        if method == "account/read":
            return (
                {"account": {"id": "fixture-account"}}
                if self.model_tools
                else {"account": None}
            )
        if method == "getAuthStatus":
            return (
                {"authMethod": "chatgpt", "authToken": None}
                if self.model_tools
                else {"authMethod": None, "authToken": None}
            )
        raise AssertionError(f"unexpected verifier method {method}")

    def run(self):
        try:
            with self.listener.accept()[0] as connection:
                connection.settimeout(5)
                stream = connection.makefile("rb")
                headers = []
                while line := stream.readline():
                    if line == b"\r\n":
                        break
                    headers.append(line)
                key = next(
                    line.split(b":", 1)[1].strip()
                    for line in headers
                    if line.lower().startswith(b"sec-websocket-key:")
                )
                accept = base64.b64encode(
                    hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest()
                )
                connection.sendall(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: "
                    + accept
                    + b"\r\n\r\n"
                )

                def send_json(value):
                    data = json.dumps(value).encode()
                    prefix = (
                        bytes([0x81, len(data)])
                        if len(data) < 126
                        else b"\x81\x7e" + struct.pack("!H", len(data))
                    )
                    connection.sendall(prefix + data)

                while header := stream.read(2):
                    length = header[1] & 127
                    if length == 126:
                        length = struct.unpack("!H", stream.read(2))[0]
                    mask = stream.read(4)
                    payload = stream.read(length)
                    request = json.loads(
                        bytes(
                            byte ^ mask[index % 4] for index, byte in enumerate(payload)
                        )
                    )
                    if "id" not in request:
                        continue
                    method = request["method"]
                    self.methods.append(method)
                    self.requests.append(request)
                    if (
                        method == "localUsage/summary"
                        and not request["params"].get("includeReview")
                        and self.pending_headlines
                    ):
                        self.pending_headlines -= 1
                        send_json(
                            {
                                "id": request["id"],
                                "error": {
                                    "code": -32603,
                                    "message": "headline pending",
                                    "data": {
                                        "source": "usage-v2",
                                        "reason": "token_headline_pending",
                                        "retryable": True,
                                    },
                                },
                            }
                        )
                        continue
                    response = self.mutate(
                        method, copy.deepcopy(self.result(method, request["params"]))
                    )
                    if method == "turn/start" and self.model_tools:
                        self.turn_count += 1
                        if self.turn_count == 1:
                            self.binding_revision = 1
                            calls = [
                                (
                                    "work-a",
                                    "usage_work",
                                    {
                                        "action": "set",
                                        "native_project_id": "fixture-project",
                                        "workstream_id": "fixture-workstream-a",
                                        "outcome_id": "fixture-outcome-a",
                                        "expected_revision": 0,
                                    },
                                ),
                                (
                                    "account-a",
                                    "account_management",
                                    {"action": "list", "limit": 1},
                                ),
                                (
                                    "usage-a",
                                    "usage_stats",
                                    {"action": "summary", "scope": "current_chat"},
                                ),
                            ]
                        elif self.turn_count == 2:
                            self.binding_revision = 2
                            calls = [
                                (
                                    "work-b",
                                    "usage_work",
                                    {
                                        "action": "set",
                                        "native_project_id": "fixture-project",
                                        "workstream_id": "fixture-workstream-b",
                                        "outcome_id": "fixture-outcome-b",
                                        "expected_revision": 1,
                                    },
                                ),
                                (
                                    "account-b",
                                    "account_management",
                                    {"action": "list", "limit": 1},
                                ),
                                (
                                    "usage-b",
                                    "usage_stats",
                                    {"action": "summary", "scope": "current_chat"},
                                ),
                            ]
                        else:
                            self.binding_revision = 3
                            calls = [
                                (
                                    "work-clear",
                                    "usage_work",
                                    {"action": "clear", "expected_revision": 2},
                                ),
                                (
                                    "usage-clear",
                                    "usage_stats",
                                    {"action": "summary", "scope": "current_chat"},
                                ),
                            ]
                        if self.code_mode:
                            receipt = {
                                "revision": self.binding_revision,
                                "transition": {
                                    "nativeProjectId": None
                                    if self.binding_revision == 3
                                    else "fixture-project"
                                },
                            }
                            account_result = {
                                "generation": 1,
                                "accounts": [],
                                "autoSelectionEnabled": False,
                                "priorityOrder": "",
                            }
                            usage_result = {
                                "schemaVersion": 1,
                                "kind": "usageSummary",
                                "snapshotGeneration": 1,
                                "pendingCorrections": 0,
                                "freshness": "fresh",
                                "watermarkAtMs": 1,
                                "counts": {},
                            }
                            raw = "\n".join(
                                json.dumps(value)
                                for value in (receipt, account_result, usage_result)
                            )
                            send_json(
                                {
                                    "method": "rawResponseItem/completed",
                                    "params": {
                                        "item": {
                                            "type": "function_call",
                                            "call_id": f"exec-{self.turn_count}",
                                            "name": "exec",
                                            "arguments": json.dumps(
                                                {
                                                    "cmd": "usage_work and accounting verification"
                                                }
                                            ),
                                        }
                                    },
                                }
                            )
                            send_json(
                                {
                                    "method": "rawResponseItem/completed",
                                    "params": {
                                        "item": {
                                            "type": "function_call_output",
                                            "call_id": f"exec-{self.turn_count}",
                                            "output": raw,
                                        }
                                    },
                                }
                            )
                        else:
                            for call_id, name, arguments in calls:
                                send_json(
                                    {
                                        "method": "rawResponseItem/completed",
                                        "params": {
                                            "item": {
                                                "type": "function_call",
                                                "call_id": call_id,
                                                "name": name,
                                                "arguments": json.dumps(arguments),
                                            }
                                        },
                                    }
                                )
                                if name == "account_management":
                                    output = {
                                        "generation": 1,
                                        "accounts": [],
                                        "autoSelectionEnabled": False,
                                        "priorityOrder": "",
                                    }
                                elif name == "usage_stats":
                                    output = {
                                        "schemaVersion": 1,
                                        "kind": "usageSummary",
                                        "snapshotGeneration": 1,
                                        "pendingCorrections": 0,
                                        "freshness": "fresh",
                                        "watermarkAtMs": 1,
                                        "counts": {},
                                    }
                                else:
                                    output = {
                                        "revision": arguments.get(
                                            "expected_revision", 0
                                        )
                                        + 1
                                    }
                                send_json(
                                    {
                                        "method": "rawResponseItem/completed",
                                        "params": {
                                            "item": {
                                                "type": "function_call_output",
                                                "call_id": call_id,
                                                "output": json.dumps(output),
                                            }
                                        },
                                    }
                                )
                        send_json(
                            {
                                "method": "rawResponseItem/completed",
                                "params": {
                                    "item": {
                                        "type": "message",
                                        "content": [
                                            {
                                                "type": "output_text",
                                                "text": "LOCAL_DEPLOY_VERIFIED",
                                            }
                                        ],
                                    }
                                },
                            }
                        )
                        send_json(
                            {
                                "method": "turn/completed",
                                "params": {"turn": {"status": "completed"}},
                            }
                        )
                    send_json({"id": request["id"], "result": response})
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            self.error = error


class LocalDeployRpcTests(unittest.TestCase):
    def plan(self, home):
        return {
            "codexHome": str(home),
            "state": str(home / "proof"),
            "verificationCwd": str(Path(__file__).resolve().parent.parent),
            "version": "0.166.0+multi.3",
            "verification": "local-rpc",
        }

    def sparse_plan(self, home):
        plan = self.plan(home)
        plan.update(
            verification="model-tools",
            requireSparseBindings=True,
            sparseBindings={
                "nativeProjectId": "fixture-project",
                "workstreamAId": "fixture-workstream-a",
                "outcomeAId": "fixture-outcome-a",
                "workstreamBId": "fixture-workstream-b",
                "outcomeBId": "fixture-outcome-b",
            },
        )
        return plan

    def test_negotiates_usage_v2_and_reads_summary_over_unix_websocket(self):
        with tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
            home = Path(temporary)
            with Peer(home) as peer:
                verify(self.plan(home))
            proof = json.loads((home / "proof/rpc.json").read_text())
            self.assertTrue(proof["localUsageSummary"])
            self.assertTrue(proof["usageTokenHeadlineCapability"])
            self.assertTrue(proof["usageTokenHeadlineProjection"])
            self.assertFalse(proof["usageSparseControlCapability"])
            self.assertEqual(proof["usageStorageFormatVersion"], 3)
            self.assertTrue(proof["localUsageRepositoryIdentity"])
            self.assertTrue(proof["usageWorkContextBindingRequest"])
            self.assertTrue(proof["usageOutcomeSchema"])
            self.assertEqual(proof["usageOutcomeEvidence"], "contract-only")
            self.assertIn("localUsage/summary", peer.methods)
            self.assertIn("localUsage/repositoryIdentity", peer.methods)
            reviews = [
                request["params"]
                for request in peer.requests
                if request["method"] == "localUsage/summary"
                and request["params"].get("includeReview")
            ]
            self.assertEqual(len(reviews), 1)
            review = reviews[0]
            self.assertEqual(review["repositoryId"], "fixture-repository")
            self.assertEqual(review["workstreamId"], "local-deploy-verification")
            self.assertLess(review["fromAtMs"], review["toAtMs"])
            self.assertEqual(len(review["workContextBindings"]), 1)
            self.assertEqual(
                review["workContextBindings"][0]["workContext"]["workstreamId"],
                review["workstreamId"],
            )
            self.assertNotIn("local-deploy-unobserved-operation", json.dumps(proof))
            self.assertNotIn("legacyUsageCapabilityNull", proof)

    def test_waits_only_for_normal_headline_publication(self):
        with tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
            home = Path(temporary)
            with Peer(home, pending_headlines=2) as peer:
                verify(self.plan(home))
            self.assertEqual(peer.methods.count("localUsage/summary"), 4)
            self.assertTrue((home / "proof/rpc.json").exists())
        with tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
            home = Path(temporary)
            with (
                Peer(home, pending_headlines=1),
                patch("local_deploy_rpc.HEADLINE_READY_TIMEOUT", 0),
            ):
                with self.assertRaisesRegex(RuntimeError, "token-headline-not-ready"):
                    verify(self.plan(home))
            self.assertFalse((home / "proof/rpc.json").exists())

    def test_accepts_known_zero_unknown_empty_and_signed_token_headlines(self):
        for state in ("zero", "unknown", "empty", "signed"):
            with (
                self.subTest(state=state),
                tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary,
            ):
                home = Path(temporary)

                def change(method, value):
                    if (
                        method != "localUsage/summary"
                        or value["report"].get("projection") != "token_headline"
                    ):
                        return value
                    report, native = value["report"], value["summary"]
                    rows, coverage = report["providerTokens"], report["coverage"]
                    details = coverage["details"]
                    if state == "unknown":
                        rows[0].update(exactTokens=None, unknownObservations=1)
                        native["providerTotalTokens"] = None
                        details["providerTotalUnknowns"] = 1
                        coverage.update(
                            state="partial", hasGaps=True, providerTotals="partial"
                        )
                    elif state == "signed":
                        for index, key, amount in [
                            (1, "inputTokens", -4),
                            (2, "cachedInputTokens", -2),
                            (3, "outputTokens", -2),
                            (4, "reasoningOutputTokens", -1),
                        ]:
                            rows[index].update(
                                measuredTokens=amount, exactTokens=amount
                            )
                            native[key] = amount
                    else:
                        observations = 0 if state == "empty" else 2
                        for row in rows:
                            row.update(
                                measuredTokens=0,
                                exactTokens=0 if observations else None,
                                unknownObservations=0,
                                observationCount=observations,
                            )
                        for key in (
                            "inputTokens",
                            "cachedInputTokens",
                            "outputTokens",
                            "reasoningOutputTokens",
                        ):
                            native[key] = 0
                        native.update(
                            providerTotalTokens=0 if observations else None,
                            cacheWriteInputTokens=0 if observations else None,
                            tokenObservations=observations,
                        )
                        details.update(
                            tokenObservations=observations, cacheWriteUnknowns=0
                        )
                        coverage.update(
                            state="complete" if observations else "unobserved",
                            hasGaps=not observations,
                        )
                    return value

                with Peer(home, change):
                    verify(self.plan(home))

    def test_accepts_positive_zero_and_unknown_outcome_unions_and_nullable_cost_counters(
        self,
    ):
        for measured, exact, unknown in (
            (123, 123, 0),
            (0, 0, 0),
            (45, None, 2),
            (0, None, 1),
        ):
            with (
                self.subTest(measured=measured, exact=exact, unknown=unknown),
                tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary,
            ):
                home = Path(temporary)

                def change(method, value):
                    if method == "localUsage/summary" and isinstance(
                        value.get("review"), dict
                    ):
                        for effort in (
                            value["review"]["outcomes"][key]
                            for key in ("totals", "attributed", "unattributed")
                        ):
                            for key in ("activeAgentMs", "elapsedExecutionMs"):
                                effort[key] = {
                                    "measured": measured,
                                    "exact": exact,
                                    "unknown": unknown,
                                }
                        cost = value["review"]["totals"]["cost"]
                        cost.update(
                            input_tokens=-4,
                            cached_input_tokens=-2,
                            uncached_input_tokens=-2,
                            output_tokens=-2,
                            reasoning_tokens=-1,
                        )
                    return value

                with Peer(home, change):
                    verify(self.plan(home))

    def test_accepts_sparse_control_capability_without_mutating_the_binding(self):
        for required in (False, True):
            with (
                self.subTest(requireSparseBindings=required),
                tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary,
            ):
                home = Path(temporary)

                def advertise(method, value):
                    if method == "initialize":
                        value["localUsageAccounting"]["methods"].extend(
                            ["localUsage/bindingTransition", "localUsage/bindingStatus"]
                        )
                    return value

                plan = self.plan(home)
                plan["requireSparseBindings"] = required
                with Peer(home, advertise) as peer:
                    verify(plan)
                proof = json.loads((home / "proof/rpc.json").read_text())
                self.assertTrue(proof["usageSparseControlCapability"])
                self.assertTrue(proof["localUsageSummary"])
                self.assertTrue(proof["accountAuthStatusAgrees"])
                self.assertNotIn("localUsage/bindingTransition", peer.methods)
                self.assertNotIn("localUsage/bindingStatus", peer.methods)

    def test_rejects_read_only_capability_when_sparse_bindings_are_required(self):
        with tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
            home = Path(temporary)
            plan = self.plan(home)
            plan["requireSparseBindings"] = True
            with Peer(home) as peer:
                with self.assertRaisesRegex(
                    RuntimeError, "usage-sparse-control-capability-missing"
                ):
                    verify(plan)
            self.assertFalse((home / "proof/rpc.json").exists())
            self.assertNotIn("localUsage/bindingTransition", peer.methods)

    def test_model_tools_requires_real_sparse_declarations_and_report(self):
        with tempfile.TemporaryDirectory(
            prefix="ld-model-sparse-", dir="/tmp"
        ) as temporary:
            home = Path(temporary)
            with Peer(home, model_tools=True) as peer:
                verify(self.sparse_plan(home))
            proof = json.loads((home / "proof/rpc.json").read_text())
            self.assertTrue(proof["realModelTurn"])
            self.assertTrue(proof["sparseJourney"])
            self.assertTrue(proof["sparseReportVerified"])
            self.assertEqual(proof["sparseDeclarationsObserved"], 3)
            self.assertGreaterEqual(proof["sparseBoundOperations"], 1)
            self.assertGreaterEqual(proof["sparseUnattributedOperations"], 1)
            self.assertEqual(
                proof["sparseOutcomeRows"],
                ["fixture-outcome-a", "fixture-outcome-b"],
            )
            self.assertIn("thread/start", peer.methods)
            self.assertIn("turn/start", peer.methods)
            self.assertIn("localUsage/summary", peer.methods)

    def test_model_tools_code_mode_uses_status_barriers_before_report(self):
        with tempfile.TemporaryDirectory(
            prefix="ld-model-sparse-code-", dir="/tmp"
        ) as temporary:
            home = Path(temporary)
            with Peer(home, model_tools=True, code_mode=True) as peer:
                verify(self.sparse_plan(home))
            proof = json.loads((home / "proof/rpc.json").read_text())
            self.assertTrue(proof["sparseReportVerified"])
            self.assertEqual(proof["sparseDeclarationsObserved"], 3)
            self.assertEqual(peer.methods.count("localUsage/bindingStatus"), 3)
            self.assertTrue(
                any(request["method"] == "turn/start" for request in peer.requests)
            )

    def test_model_tools_rejects_control_receipts_without_real_outcome_report(self):
        with tempfile.TemporaryDirectory(
            prefix="ld-model-sparse-", dir="/tmp"
        ) as temporary:
            home = Path(temporary)

            def missing_report(method, value):
                if (
                    method == "localUsage/summary"
                    and value.get("report", {}).get("scope", {}).get("type") == "thread"
                ):
                    value.pop("review", None)
                    value["report"].pop("workBindings", None)
                return value

            with Peer(home, missing_report, model_tools=True):
                with self.assertRaisesRegex(RuntimeError, "real-sparse-report-invalid"):
                    verify(self.sparse_plan(home))
            self.assertFalse((home / "proof/rpc.json").exists())

    def test_rejects_incompatible_usage_and_inconsistent_snapshot_without_private_payload(
        self,
    ):
        cases = [
            ("initialize", lambda value: value.update(localUsageAccounting=None)),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(version=99),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(version=True),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(storage="sqlite"),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(methods=[]),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(
                    methods=["localUsage/summary"]
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"]["methods"].append(
                    "localUsage/legacy"
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"]["methods"].append(
                    "localUsage/bindingTransition"
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"]["methods"].append(
                    "localUsage/bindingStatus"
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"]["methods"].append(
                    "localUsage/summary"
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"]["methods"].extend(
                    [
                        "localUsage/bindingTransition",
                        "localUsage/bindingStatus",
                        "localUsage/bindingTransition",
                    ]
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"]["methods"].extend(
                    [
                        "localUsage/bindingTransition",
                        "localUsage/bindingStatus",
                        "localUsage/legacy",
                    ]
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(methods=[{}]),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(
                    supportedVersions=[True]
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].pop("supportedVersions"),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].pop(
                    "supportsWorkContextBindings"
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(
                    supportsWorkstreamOutcomes=False
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(
                    supportsTokenHeadlines=False
                ),
            ),
            (
                "initialize",
                lambda value: value["localUsageAccounting"].update(
                    snapshotRetentionMs=0
                ),
            ),
            (
                "localUsage/repositoryIdentity",
                lambda value: value.update(formatVersion=99),
            ),
            (
                "localUsage/repositoryIdentity",
                lambda value: value.update(repositoryId=["private-payload"]),
            ),
            (
                "localUsage/summary",
                lambda value: value["report"].update(databaseSchemaVersion=99),
            ),
            (
                "localUsage/summary",
                lambda value: value["report"].update(schemaVersion=True),
            ),
            (
                "localUsage/summary",
                lambda value: value["summary"].update(projection="legacy_metadata"),
            ),
            (
                "localUsage/summary",
                lambda value: value["summary"].update(inputTokens=-1),
            ),
            (
                "localUsage/summary",
                lambda value: value["snapshot"].update(generatedAt=2001),
            ),
            (
                "localUsage/summary",
                lambda value: value["snapshot"].update(journalWatermark=1),
            ),
            (
                "localUsage/summary",
                lambda value: value["summary"].update(inputTokens="private-payload"),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["report"]["coverage"].update(counts="recorded")
                    if value["report"].get("projection") == "token_headline"
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["report"]["providerTokens"][0].update(exactTokens=121)
                    if value["report"].get("projection") == "token_headline"
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["report"].update(counts={"operations": 0})
                    if value["report"].get("projection") == "token_headline"
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["totals"]["cost"].update(priced_requests=True)
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["totals"]["cost"].update(model_requests=0)
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["totals"]["cost"].update(estimated_usd_micros=None)
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["totals"]["cost"].update(
                        unknown_observations="private-payload"
                    )
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["report"]["cost"]["actualBillingMicros"].update(exact=12)
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["outcomes"]["totals"]["activeAgentMs"].update(
                        measured=-1
                    )
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: value["report"].update(workBindings={}),
            ),
            (
                "localUsage/summary",
                lambda value: value["report"].update(scope={"type": "all", "id": None}),
            ),
            (
                "localUsage/summary",
                lambda value: value["report"].update(timeRange=None),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["outcomes"].update(schemaVersion=True)
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value.update(review=None)
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["outcomes"].update(rows="private-payload")
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
            (
                "localUsage/summary",
                lambda value: (
                    value["review"]["outcomes"]["totals"]["elapsedExecutionMs"].update(
                        exact=100
                    )
                    if isinstance(value.get("review"), dict)
                    else None
                ),
            ),
        ]
        for method, mutate in cases:
            with (
                self.subTest(method=method),
                tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary,
            ):
                home = Path(temporary)

                def change(actual, value):
                    if actual == method:
                        mutate(value)
                    return value

                with Peer(home, change):
                    with self.assertRaises(RuntimeError) as error:
                        verify(self.plan(home))
                self.assertNotIn("private-payload", str(error.exception))
                self.assertFalse((home / "proof/rpc.json").exists())

    def test_accepts_unknown_freshness_and_pending_entries_without_fabricating_timing(
        self,
    ):
        with tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
            home = Path(temporary)

            def pending(method, value):
                if method == "localUsage/summary":
                    value["snapshot"].update(freshness="unknown", pendingEntries=1)
                    if value["report"].get("projection") == "token_headline":
                        value["report"]["coverage"]["details"][
                            "latestJournalWatermark"
                        ] = 3
                return value

            with Peer(home, pending):
                verify(self.plan(home))
            proof = json.loads((home / "proof/rpc.json").read_text())
            self.assertEqual(proof["usageFreshness"], "unknown")
            self.assertEqual(proof["usagePendingEntries"], 1)
            self.assertEqual(proof["usageOutcomeEvidence"], "contract-only")

    @unittest.skipUnless(
        os.environ.get("CODEX_MULTI_VERIFY_BINARY"),
        "run after the source package is assembled",
    )
    def test_actual_candidate_in_isolated_home(self):
        with tempfile.TemporaryDirectory(prefix="ld-real-", dir="/tmp") as temporary:
            home = Path(temporary)
            for relative, contents in PRESERVED_FILES.items():
                target = home / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(contents)
            preserved = {
                relative: digest(home / relative) for relative in PRESERVED_FILES
            }
            binary = os.environ["CODEX_MULTI_VERIFY_BINARY"]
            environment = {
                name: value
                for name, value in os.environ.items()
                if name not in {"OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"}
            }
            process = subprocess.Popen(
                [binary, "app-server", "--listen", "unix://"],
                env={**environment, "CODEX_HOME": str(home), "HOME": str(home)},
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                endpoint = home / "app-server-control/app-server-control.sock"
                deadline = time.monotonic() + 30
                while not endpoint.exists() and time.monotonic() < deadline:
                    self.assertIsNone(
                        process.poll(),
                        "candidate exited before creating its local endpoint",
                    )
                    time.sleep(0.05)
                self.assertTrue(endpoint.exists())
                candidate_plan = self.plan(home)
                candidate_plan["version"] = os.environ.get(
                    "CODEX_MULTI_VERIFY_VERSION", candidate_plan["version"]
                )
                verify(candidate_plan)
                proof = json.loads((home / "proof/rpc.json").read_text())
                self.assertTrue(proof["localUsageSummary"])
                self.assertTrue(proof["accountAuthStatusAgrees"])
                self.assertEqual(
                    {relative: digest(home / relative) for relative in PRESERVED_FILES},
                    preserved,
                )
            finally:
                process.terminate()
                process.wait(timeout=30)


class LocalUsagePackagePreparationTests(unittest.TestCase):
    def test_packages_cargo_metadata_outputs_instead_of_stale_default_binaries(self):
        with tempfile.TemporaryDirectory(prefix="ld-build-", dir="/tmp") as temporary:
            root = Path(temporary)
            workspace = root / "codex-rs"
            workspace.mkdir()
            version = "0.166.0+multi.5"
            (workspace / "Cargo.toml").write_text(
                f'[workspace.package]\nversion = "{version}"\n'
            )
            target = root / "custom-target"
            names = ("codex", "codex-code-mode-host", "codex-exec-server", "bwrap")
            for directory, label in [
                (workspace / "target/debug", "stale"),
                (target / "debug", "fresh"),
            ]:
                directory.mkdir(parents=True)
                for name in names:
                    (directory / name).write_text(f"{label}-{name}")
            expected = {
                name: hashlib.sha256(f"fresh-{name}".encode()).hexdigest()
                for name in names
            }
            metadata_environment = []

            def run(arguments, **kwargs):
                command = [str(argument) for argument in arguments]
                output = ""
                if command[0] == "git":
                    output = "f" * 40 if command[1] == "rev-parse" else ""
                elif command[1] == "metadata":
                    metadata_environment.append(kwargs["env"]["CARGO_TARGET_DIR"])
                    output = json.dumps({"target_directory": str(target)})
                elif command[1] == "build":
                    pass
                elif command[1].endswith("build_codex_package.py"):
                    package = Path(command[command.index("--package-dir") + 1])
                    entrypoint = Path(command[command.index("--entrypoint-bin") + 1])
                    (package / "bin").mkdir(parents=True)
                    (package / "bin/codex").write_bytes(entrypoint.read_bytes())
                elif command[1] == "--version":
                    output = f"codex-cli {version}\n"
                else:
                    self.fail("unexpected package preparation command")
                return subprocess.CompletedProcess(command, 0, stdout=output)

            run_id = "t20261111T000000Z-a1"
            environment = {
                "DEVCOORDINATOR_RUN_ID": run_id,
                "CARGO_TARGET_DIR": "custom-target",
            }
            v8 = SimpleNamespace(archive=root / "v8.a.gz", binding=root / "binding.rs")
            with (
                patch.multiple(
                    usage_package, ROOT=root, os=SimpleNamespace(environ=environment)
                ),
                patch.object(usage_package, "run", side_effect=run),
                patch.object(
                    usage_package, "fetch_codex_v8_artifacts", return_value=v8
                ),
            ):
                usage_package.build()
            manifest = json.loads(
                (
                    workspace / "target/local-usage-package" / run_id / "candidate.json"
                ).read_text()
            )
            self.assertEqual(manifest["sourceBinaries"], expected)
            self.assertEqual(manifest["executableSha256"], expected["codex"])
            self.assertEqual(metadata_environment, ["custom-target"])


if __name__ == "__main__":
    unittest.main()
