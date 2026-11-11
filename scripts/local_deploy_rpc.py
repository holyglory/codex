#!/usr/bin/env python3
"""Verify the actual selected Unix endpoint without retaining RPC content."""

import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import sys
import time

from local_deploy_owner import save


def report_results(output):
    """Inspect bounded tool output in memory; retain only report-shape evidence."""
    found = set()
    pending = [(output, 0)]
    decoder = json.JSONDecoder()
    while pending:
        value, depth = pending.pop()
        if depth > 12:
            continue
        if isinstance(value, str):
            # Code-mode text includes a completion prefix before JSON output.
            for line in value.splitlines():
                start = line.find("{")
                if start >= 0:
                    try:
                        parsed, _ = decoder.raw_decode(line[start:])
                    except ValueError:
                        continue
                    pending.append((parsed, depth + 1))
        elif isinstance(value, list):
            pending.extend((item, depth + 1) for item in value)
        elif isinstance(value, dict):
            if value.get("isError") is True:
                continue
            if (
                type(value.get("generation")) is int
                and isinstance(value.get("accounts"), list)
                and type(value.get("autoSelectionEnabled")) is bool
                and isinstance(value.get("priorityOrder"), str)
            ):
                found.add("account_management")
            if (
                value.get("schemaVersion") == 1
                and value.get("kind") == "usageSummary"
                and type(value.get("snapshotGeneration")) is int
                and type(value.get("pendingCorrections")) is int
                and value.get("freshness") in {"unknown", "fresh", "stale"}
                and "watermarkAtMs" in value
                and isinstance(value.get("counts"), dict)
            ):
                found.add("usage_stats")
            pending.extend((item, depth + 1) for item in value.values())
    return found


class Rpc:
    def __init__(self, endpoint):
        self.connection = socket.socket(socket.AF_UNIX)
        self.connection.settimeout(30)
        self.connection.connect(str(endpoint))
        self.buffer = b""
        key = base64.b64encode(os.urandom(16)).decode()
        self.connection.sendall(
            (
                "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                f"Sec-WebSocket-Key: {key}\r\n\r\n"
            ).encode()
        )
        while b"\r\n\r\n" not in self.buffer:
            chunk = self.connection.recv(4096)
            if not chunk:
                raise RuntimeError("websocket-handshake-eof")
            self.buffer += chunk
            if len(self.buffer) > 8192:
                raise RuntimeError("websocket-header-too-large")
        header, self.buffer = self.buffer.split(b"\r\n\r\n", 1)
        accept = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
            ).digest()
        )
        if (
            not header.startswith(b"HTTP/1.1 101 ")
            or accept.lower() not in header.lower()
        ):
            raise RuntimeError("websocket-upgrade-failed")
        self.sequence = 0
        self.calls = {}
        self.outputs = {}
        self.marker = False
        self.completed = None

    def read(self, length):
        while len(self.buffer) < length:
            chunk = self.connection.recv(max(4096, length - len(self.buffer)))
            if not chunk:
                raise RuntimeError("websocket-closed")
            self.buffer += chunk
        result, self.buffer = self.buffer[:length], self.buffer[length:]
        return result

    def send(self, value, opcode=1):
        payload = json.dumps(value).encode() if opcode == 1 else value
        length = len(payload)
        marker = (
            bytes([length]) if length < 126 else b"\x7e" + struct.pack("!H", length)
        )
        mask = os.urandom(4)
        self.connection.sendall(
            bytes([0x80 | opcode, marker[0] | 0x80])
            + marker[1:]
            + mask
            + bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        )

    def receive(self):
        parts = []
        while True:
            first, second = self.read(2)
            length = second & 127
            if length == 126:
                length = struct.unpack("!H", self.read(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self.read(8))[0]
            if second & 128 or length > 1_048_576:
                raise RuntimeError("unexpected-websocket-frame")
            payload = self.read(length)
            opcode = first & 15
            if opcode == 8:
                raise RuntimeError("websocket-close-frame")
            if opcode == 9:
                self.send(payload, opcode=10)
                continue
            if opcode == 10:
                continue
            if opcode not in (0, 1):
                raise RuntimeError("unexpected-websocket-opcode")
            parts.append(payload)
            if sum(map(len, parts)) > 1_048_576:
                raise RuntimeError("rpc-message-too-large")
            if first & 128:
                value = json.loads(b"".join(parts))
                if "LOCAL_DEPLOY_VERIFIED" in json.dumps(value, separators=(",", ":")):
                    self.marker = True
                if value.get("method") == "rawResponseItem/completed":
                    item = value.get("params", {}).get("item", {})
                    if item.get("type") in ("function_call", "custom_tool_call"):
                        self.calls[item.get("call_id") or item.get("id")] = item.get(
                            "name"
                        )
                    elif item.get("type") in (
                        "function_call_output",
                        "custom_tool_call_output",
                    ):
                        call_id = item.get("call_id") or item.get("id")
                        output = item.get("output")
                        self.outputs[call_id] = {
                            "name": self.calls.get(call_id),
                            "reports": report_results(output),
                        }
                if value.get("method") == "turn/completed":
                    self.completed = value["params"]["turn"]["status"]
                return value

    def call(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        self.send({"id": request_id, "method": method, "params": params})
        while True:
            value = self.receive()
            if value.get("id") == request_id:
                if "error" in value:
                    raise RuntimeError(f"rpc-{method}-error-{value['error']['code']}")
                return value["result"]


def verify(plan):
    endpoint = Path(plan["codexHome"]) / "app-server-control/app-server-control.sock"
    rpc = Rpc(endpoint)
    try:
        verify_connection(rpc, plan)
    finally:
        rpc.connection.close()


def verify_connection(rpc, plan):
    endpoint = Path(plan["codexHome"]) / "app-server-control/app-server-control.sock"
    initialized = rpc.call(
        "initialize",
        {
            "clientInfo": {"name": "local_deploy_verification", "version": "1"},
            "capabilities": {"experimentalApi": True},
        },
    )
    rpc.send({"method": "initialized", "params": {}})
    if plan["version"] not in initialized["userAgent"] or Path(
        initialized["codexHome"]
    ) != Path(plan["codexHome"]):
        raise RuntimeError("initialize-identity-mismatch")
    capability = initialized.get("multiAccount")
    if (
        not isinstance(capability, dict)
        or capability.get("supportsCreditUsage") is not True
    ):
        raise RuntimeError("account-credit-capability-missing")
    usage = initialized.get("localUsageAccounting")
    read_methods = ["localUsage/repositoryIdentity", "localUsage/summary"]
    sparse_methods = [
        "localUsage/bindingStatus",
        "localUsage/bindingTransition",
        *read_methods,
    ]
    if (
        not isinstance(usage, dict)
        or type(usage.get("version")) is not int
        or usage["version"] != 1
        or usage.get("storage") != "usage-v2"
        or not isinstance(usage.get("supportedVersions"), list)
        or usage["supportedVersions"] != [1]
        or any(type(version) is not int for version in usage["supportedVersions"])
        or not isinstance(usage.get("methods"), list)
        or any(not isinstance(method, str) for method in usage["methods"])
        or sorted(usage["methods"]) not in (read_methods, sparse_methods)
        or usage.get("supportsWorkContextBindings") is not True
        or usage.get("supportsWorkstreamOutcomes") is not True
        or type(usage.get("snapshotRetentionMs")) is not int
        or usage["snapshotRetentionMs"] <= 0
    ):
        raise RuntimeError("usage-v2-capability-invalid")
    sparse_controls = sorted(usage["methods"]) == sparse_methods
    require_sparse = plan.get("requireSparseBindings", False)
    if type(require_sparse) is not bool:
        raise RuntimeError("usage-sparse-control-requirement-invalid")
    if require_sparse and not sparse_controls:
        raise RuntimeError("usage-sparse-control-capability-missing")
    identity = rpc.call(
        "localUsage/repositoryIdentity",
        {
            "contractVersion": 1,
            "path": plan["verificationCwd"],
        },
    )
    if (
        not isinstance(identity, dict)
        or type(identity.get("contractVersion")) is not int
        or identity["contractVersion"] != 1
        or identity.get("source") != "usage-v2"
        or type(identity.get("formatVersion")) is not int
        or identity["formatVersion"] != 3
        or "repositoryId" not in identity
        or identity["repositoryId"] is not None
        and (
            not isinstance(identity["repositoryId"], str)
            or not identity["repositoryId"]
            or len(identity["repositoryId"]) > 256
            or any(ord(character) < 32 for character in identity["repositoryId"])
        )
    ):
        raise RuntimeError("usage-v2-repository-identity-invalid")
    summary = rpc.call("localUsage/summary", {"contractVersion": 1})
    report = summary.get("report", {})
    snapshot = summary.get("snapshot", {})
    native = summary.get("summary", {})
    if (
        type(summary.get("contractVersion")) is not int
        or summary["contractVersion"] != 1
        or summary.get("source") != "usage-v2"
        or type(report.get("schemaVersion")) is not int
        or report["schemaVersion"] != 1
        or report.get("kind") != "usageSummary"
        or report.get("databaseSchemaVersion") != 3
        or report.get("scope") != {"type": "all", "id": None}
        or report.get("coverage", {}).get("state")
        not in {"complete", "partial", "unobserved"}
        or type(native.get("schemaVersion")) is not int
        or native["schemaVersion"] != 1
        or native.get("kind") != "usageSummary"
        or any(
            type(native.get(key)) is not int or native[key] < 0
            for key in (
                "records",
                "inputTokens",
                "cachedInputTokens",
                "outputTokens",
                "reasoningOutputTokens",
            )
        )
        or any(
            type(report.get("counts", {}).get(key)) is not int
            or report["counts"][key] < 0
            for key in ("operations", "modelRequests", "tools")
        )
        or any(
            type(snapshot.get(key)) is not int or snapshot[key] < 0
            for key in (
                "generation",
                "watermark",
                "journalWatermark",
                "generatedAt",
                "watermarkAtMs",
                "updatedAtMs",
                "pendingEntries",
                "pendingCorrections",
            )
        )
        or type(summary.get("generatedAt")) is not int
        or snapshot.get("generatedAt", 0) > summary.get("generatedAt", 0)
        or snapshot.get("watermark", 0) > snapshot.get("journalWatermark", 0)
        or snapshot.get("freshness") not in {"unknown", "fresh", "stale"}
        or not isinstance(snapshot.get("sourceWatermark"), str)
        or not snapshot["sourceWatermark"]
        or len(snapshot["sourceWatermark"]) > 256
    ):
        raise RuntimeError("usage-v2-summary-invalid")
    # An unmatched, content-free declaration proves request acceptance without
    # manufacturing live operation or outcome evidence. Real binding joins are
    # covered by the isolated app-server fixture journey.
    captured_at = int(time.time() * 1000)
    review_params = {
        "contractVersion": 1,
        "fromAtMs": max(0, captured_at - 60_000),
        "toAtMs": captured_at + 1,
        "includeReview": True,
        "workstreamId": "local-deploy-verification",
        "workContextBindings": [
            {
                "schemaVersion": 1,
                "operationId": "local-deploy-unobserved-operation",
                "capturedAtMs": captured_at,
                "workContext": {
                    "schemaVersion": 1,
                    "id": "local-deploy-unobserved-context",
                    "workstreamId": "local-deploy-verification",
                    "outcomeId": "local-deploy-unobserved-outcome",
                },
            }
        ],
    }
    if identity["repositoryId"] is not None:
        review_params["repositoryId"] = identity["repositoryId"]
    scoped = rpc.call("localUsage/summary", review_params)
    if (
        not isinstance(scoped, dict)
        or type(scoped.get("contractVersion")) is not int
        or scoped["contractVersion"] != 1
        or scoped.get("source") != "usage-v2"
        or not isinstance(scoped.get("report"), dict)
        or scoped["report"].get("scope")
        != {
            "type": "repository" if identity["repositoryId"] is not None else "all",
            "id": identity["repositoryId"],
        }
        or scoped["report"].get("timeRange")
        != {
            "startMs": review_params["fromAtMs"],
            "endMs": review_params["toAtMs"],
        }
        or not isinstance(scoped.get("review"), dict)
        or not isinstance(scoped.get("workContexts"), list)
        or not isinstance(scoped.get("operationBindings"), list)
    ):
        raise RuntimeError("usage-v2-review-invalid")
    bindings = scoped["report"].get("workBindings")
    outcomes = scoped["review"].get("outcomes")
    if (
        not isinstance(bindings, dict)
        or type(bindings.get("schemaVersion")) is not int
        or bindings["schemaVersion"] != 1
        or bindings.get("basis")
        not in {"request_declarations", "request_and_compatibility_declarations"}
        or type(bindings.get("declaredOperations")) is not int
        or not 1 <= bindings["declaredOperations"] <= 256
        or not isinstance(outcomes, dict)
        or type(outcomes.get("schemaVersion")) is not int
        or outcomes["schemaVersion"] != 1
        or outcomes.get("coverage") not in {"unattributed", "partial", "complete"}
        or not isinstance(outcomes.get("rows"), list)
        or any(not isinstance(row, dict) for row in outcomes["rows"])
        or any(
            row.get("workstreamId") != review_params["workstreamId"]
            for row in outcomes["rows"]
        )
    ):
        raise RuntimeError("usage-v2-outcome-contract-invalid")
    efforts = [outcomes.get(key) for key in ("totals", "attributed", "unattributed")]
    efforts.extend(row.get("effort") for row in outcomes["rows"])
    for effort in efforts:
        if not isinstance(effort, dict):
            raise RuntimeError("usage-v2-outcome-effort-invalid")
        for key in ("activeAgentMs", "elapsedExecutionMs"):
            measurement = effort.get(key)
            if (
                not isinstance(measurement, dict)
                or type(measurement.get("measured")) is not int
                or measurement["measured"] != 0
                or "exact" not in measurement
                or measurement["exact"] is not None
                or type(measurement.get("unknown")) is not int
                or measurement["unknown"] < 0
            ):
                raise RuntimeError("usage-v2-outcome-timing-invalid")
    models = rpc.call("model/list", {"limit": 1})
    rpc.call("skills/list", {"cwds": [plan["verificationCwd"]]})
    profiles = rpc.call("accountProfile/list", {"limit": 25})
    account = rpc.call("account/read", {"refreshToken": False})
    auth = rpc.call("getAuthStatus", {"includeToken": False, "refreshToken": False})
    if bool(account.get("account")) != bool(auth.get("authMethod")):
        raise RuntimeError("account-auth-status-disagrees")
    if auth.get("authToken"):
        raise RuntimeError("auth-status-returned-unrequested-token")
    if not isinstance(models.get("data"), list) or not isinstance(
        profiles.get("data"), list
    ):
        raise RuntimeError("catalog-response-invalid")
    if any(
        type(profile.get("creditUsageEnabled")) is not bool
        for profile in profiles["data"]
    ):
        raise RuntimeError("account-credit-profile-field-missing")
    proof = {
        "result": "passed",
        "initialize": True,
        "modelList": True,
        "skillsList": True,
        "accountProfileList": True,
        "creditCapability": True,
        "creditProfileFields": True,
        "localUsageCapability": True,
        "usageSparseControlCapability": sparse_controls,
        "localUsageRepositoryIdentity": True,
        "localUsageSummary": True,
        "usageWorkContextBindingRequest": True,
        "usageOutcomeSchema": True,
        "usageOutcomeEvidence": "contract-only",
        "usageContractVersion": 1,
        "usageStorageFormatVersion": 3,
        "usageFreshness": snapshot["freshness"],
        "usagePendingEntries": snapshot["pendingEntries"],
        "usagePendingCorrections": snapshot["pendingCorrections"],
        "accountAuthStatusAgrees": True,
        "realModelTurn": False,
        "usageToolRequested": False,
        "checkedAtMs": int(time.time() * 1000),
    }
    if plan.get("verification") == "model-tools":
        if account.get("account") is None:
            raise RuntimeError("preserved-account-login-unavailable")
        thread = rpc.call(
            "thread/start",
            {
                "cwd": plan["verificationCwd"],
                "approvalPolicy": "never",
                "sandbox": "read-only",
                "experimentalRawEvents": True,
                "baseInstructions": "Perform only the requested verification. Do not edit files.",
            },
        )["thread"]
        rpc.call(
            "turn/start",
            {
                "threadId": thread["id"],
                "input": [
                    {
                        "type": "text",
                        "text": "Call account_management with action list and limit 1. Call usage_stats with action summary and scope current_chat. "
                        "If using code mode, print both actual tool return values with text({account_management: accountResult, usage_stats: usageResult}); "
                        "do not replace them with booleans or a summary. Do not print credentials or file contents. "
                        "After both tools return successfully, reply exactly LOCAL_DEPLOY_VERIFIED. If a tool fails, report failure.",
                    }
                ],
            },
        )
        deadline = time.monotonic() + 180
        while not rpc.completed and time.monotonic() < deadline:
            rpc.receive()
        required = {"account_management", "usage_stats"}
        direct = set()
        wrapped = set()
        for output in rpc.outputs.values():
            name = (output["name"] or "").rsplit(".", 1)[-1]
            if name in required and name in output["reports"]:
                direct.add(name)
            elif name == "exec":
                wrapped.update(output["reports"])
        if (
            rpc.completed != "completed"
            or not rpc.marker
            or not required <= direct | wrapped
        ):
            raise RuntimeError("real-model-tools-not-completed")
        proof.update(
            realModelTurn=True,
            usageToolRequested=True,
            accountToolRequested=True,
            toolExecutionMode="direct" if required <= direct else "code-mode-wrapper",
            nestedToolResultsObserved=True,
            markerObserved=rpc.marker,
            threadId=thread["id"],
        )
        original_id = plan.get("originalThreadId")
        if original_id:
            resumed = rpc.call(
                "thread/resume", {"threadId": original_id, "excludeTurns": True}
            )
            if resumed.get("thread", {}).get("id") != original_id:
                raise RuntimeError("original-chat-resume-mismatch")
            second = Rpc(endpoint)
            try:
                second.call(
                    "initialize",
                    {
                        "clientInfo": {
                            "name": "local_reconnect_verification",
                            "version": "1",
                        },
                        "capabilities": {"experimentalApi": True},
                    },
                )
                second.send({"method": "initialized", "params": {}})
                reattached = second.call(
                    "thread/resume", {"threadId": original_id, "excludeTurns": True}
                )
                if reattached.get("thread", {}).get("id") != original_id:
                    raise RuntimeError("second-client-chat-resume-mismatch")
            finally:
                second.connection.close()
            proof["originalChatResumed"] = True
            proof["secondClientReattached"] = True
    save(Path(plan["state"]) / "rpc.json", proof)


if __name__ == "__main__":
    verify(json.loads(Path(sys.argv[1]).read_text()))
