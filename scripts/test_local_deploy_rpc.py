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
import unittest

from local_deploy_rpc import verify


class Peer:
    def __init__(self, home, mutate=None):
        self.home = home
        self.mutate = mutate or (lambda method, value: value)
        endpoint = home / "app-server-control/app-server-control.sock"
        endpoint.parent.mkdir()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(endpoint))
        self.listener.listen(1)
        self.listener.settimeout(5)
        self.methods = []
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

    def result(self, method):
        if method == "initialize":
            return {"userAgent": "fixture/0.166.0+multi.3", "codexHome": str(self.home),
                    "multiAccount": {"supportsCreditUsage": True},
                    "localUsageAccounting": {"version": 1, "supportedVersions": [1],
                       "methods": ["localUsage/summary", "localUsage/repositoryIdentity"],
                       "storage": "usage-v2", "supportsWorkContextBindings": True,
                       "supportsWorkstreamOutcomes": True, "snapshotRetentionMs": 5000}}
        if method == "localUsage/summary":
            return {"contractVersion": 1, "source": "usage-v2", "generatedAt": 2000,
                    "snapshot": {"sourceWatermark": "usage-v2:fixture", "generation": 3,
                        "watermark": 2, "journalWatermark": 3, "generatedAt": 1999,
                        "watermarkAtMs": 1000, "updatedAtMs": 1000, "freshness": "fresh",
                        "pendingEntries": 0, "pendingCorrections": 0, "refreshId": None},
                    "report": {"schemaVersion": 1, "kind": "usageSummary", "databaseSchemaVersion": 3,
                        "taxonomyVersion": 1, "scope": {"type": "all", "id": None}, "timeRange": None,
                        "coverage": {"state": "partial", "hasGaps": True},
                        "counts": {"operations": 1, "modelRequests": 1, "tools": 0}},
                    "summary": {"schemaVersion": 1, "kind": "usageSummary", "records": 2,
                        "inputTokens": 100, "cachedInputTokens": 50, "outputTokens": 20,
                        "reasoningOutputTokens": 5, "counts": {"operations": 1, "modelRequests": 1, "tools": 0}},
                    "workContexts": [], "operationBindings": []}
        if method in {"model/list", "accountProfile/list"}:
            return {"data": []}
        if method == "skills/list":
            return {"data": []}
        if method == "account/read":
            return {"account": None}
        if method == "getAuthStatus":
            return {"authMethod": None, "authToken": None}
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
                key = next(line.split(b":", 1)[1].strip() for line in headers if line.lower().startswith(b"sec-websocket-key:"))
                accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
                while header := stream.read(2):
                    length = header[1] & 127
                    if length == 126:
                        length = struct.unpack("!H", stream.read(2))[0]
                    mask = stream.read(4)
                    payload = stream.read(length)
                    request = json.loads(bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload)))
                    if "id" not in request:
                        continue
                    method = request["method"]
                    self.methods.append(method)
                    response = self.mutate(method, copy.deepcopy(self.result(method)))
                    data = json.dumps({"id": request["id"], "result": response}).encode()
                    prefix = bytes([0x81, len(data)]) if len(data) < 126 else b"\x81\x7e" + struct.pack("!H", len(data))
                    connection.sendall(prefix + data)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            self.error = error


class LocalDeployRpcTests(unittest.TestCase):
    def plan(self, home):
        return {"codexHome": str(home), "state": str(home / "proof"),
                "verificationCwd": str(Path(__file__).resolve().parent.parent),
                "version": "0.166.0+multi.3", "verification": "local-rpc"}

    def test_negotiates_usage_v2_and_reads_summary_over_unix_websocket(self):
        with tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
            home = Path(temporary)
            with Peer(home) as peer:
                verify(self.plan(home))
            proof = json.loads((home / "proof/rpc.json").read_text())
            self.assertTrue(proof["localUsageSummary"])
            self.assertEqual(proof["usageStorageFormatVersion"], 3)
            self.assertIn("localUsage/summary", peer.methods)
            self.assertNotIn("legacyUsageCapabilityNull", proof)

    def test_rejects_incompatible_usage_and_inconsistent_snapshot_without_private_payload(self):
        cases = [
            ("initialize", lambda value: value.update(localUsageAccounting=None)),
            ("initialize", lambda value: value["localUsageAccounting"].update(version=99)),
            ("initialize", lambda value: value["localUsageAccounting"].update(storage="sqlite")),
            ("initialize", lambda value: value["localUsageAccounting"].update(methods=[])),
            ("localUsage/summary", lambda value: value["report"].update(databaseSchemaVersion=99)),
            ("localUsage/summary", lambda value: value["snapshot"].update(generatedAt=2001)),
            ("localUsage/summary", lambda value: value["snapshot"].update(journalWatermark=1)),
            ("localUsage/summary", lambda value: value["summary"].update(inputTokens="private-payload")),
        ]
        for method, mutate in cases:
            with self.subTest(method=method), tempfile.TemporaryDirectory(prefix="ld-rpc-", dir="/tmp") as temporary:
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

    @unittest.skipUnless(os.environ.get("CODEX_MULTI_VERIFY_BINARY"), "run after the source package is assembled")
    def test_actual_candidate_in_isolated_home(self):
        with tempfile.TemporaryDirectory(prefix="ld-real-", dir="/tmp") as temporary:
            home = Path(temporary)
            binary = os.environ["CODEX_MULTI_VERIFY_BINARY"]
            process = subprocess.Popen([binary, "app-server", "--listen", "unix://"],
                env={**os.environ, "CODEX_HOME": str(home), "HOME": str(home)},
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                endpoint = home / "app-server-control/app-server-control.sock"
                deadline = time.monotonic() + 30
                while not endpoint.exists() and time.monotonic() < deadline:
                    self.assertIsNone(process.poll(), "candidate exited before creating its local endpoint")
                    time.sleep(0.05)
                self.assertTrue(endpoint.exists())
                candidate_plan = self.plan(home)
                candidate_plan["version"] = os.environ.get("CODEX_MULTI_VERIFY_VERSION", candidate_plan["version"])
                verify(candidate_plan)
                proof = json.loads((home / "proof/rpc.json").read_text())
                self.assertTrue(proof["localUsageSummary"])
            finally:
                process.terminate()
                process.wait(timeout=30)


if __name__ == "__main__":
    unittest.main()
