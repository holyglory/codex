"""Exercise network incident capture through a real, isolated Codex app-server."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile

from app_server_harness import AppServerHarness
from app_server_harness import _ResponsesHandler
from app_server_harness import ev_assistant_message
from app_server_harness import ev_completed
from app_server_harness import ev_response_created
from app_server_harness import sse
from openai_codex import ApprovalMode, Codex, CodexConfig, Sandbox


class IncidentHandler(_ResponsesHandler):
    """Fail real WebSocket upgrades/streams without using an external provider."""

    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.headers.get("Upgrade", "").lower() != "websocket":
            return super().do_GET()
        key = self.headers["Sec-WebSocket-Key"]
        accept = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
            ).digest()
        ).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.send_header("x-request-id", "req-websocket-fixture")
        self.end_headers()
        self.connection.settimeout(10)
        try:
            header = self.rfile.read(2)
            if len(header) != 2:
                return
            length = header[1] & 127
            if length == 126:
                length = struct.unpack("!H", self.rfile.read(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self.rfile.read(8))[0]
            if header[1] & 128:
                self.rfile.read(4)
            # Discard the request payload; this verifier does not need its content.
            while length:
                chunk = self.rfile.read(min(length, 65536))
                if not chunk:
                    return
                length -= len(chunk)
            self.frame(
                1, json.dumps(ev_response_created("resp-websocket-fixture")).encode()
            )
            self.server.mock.websocket_requests += 1
            if self.server.mock.healthy_websocket:
                self.frame(
                    1, json.dumps(ev_assistant_message("msg-ws-ok", "WS_OK")).encode()
                )
                self.frame(
                    1, json.dumps(ev_completed("resp-websocket-fixture")).encode()
                )
                self.frame(8, struct.pack("!H", 1000))
            elif self.server.mock.websocket_requests == 1:
                self.frame(
                    1,
                    json.dumps(
                        {
                            "type": "error",
                            "status": 503,
                            "error": {
                                "code": "fixture_overloaded",
                                "message": "PRIVATE_RESPONSE_BODY",
                            },
                            "headers": {
                                "x-request-id": "req-wrapped-websocket-fixture"
                            },
                        }
                    ).encode(),
                )
            else:
                self.frame(
                    8,
                    struct.pack("!H", 1008)
                    + b"policy violation token=synthetic-secret",
                )
        except (OSError, socket.timeout):
            return
        finally:
            self.close_connection = True

    def frame(self, opcode, payload):
        size = len(payload)
        prefix = bytes([0x80 | opcode])
        prefix += (
            bytes([size]) if size < 126 else bytes([126]) + struct.pack("!H", size)
        )
        self.wfile.write(prefix + payload)
        self.wfile.flush()

    def do_POST(self):
        if self.server.mock.fail_next_http:
            self.server.mock.fail_next_http = False
            self.rfile.read(int(self.headers.get("content-length", "0")))
            body = (
                b'{"error":{"message":"PRIVATE_RESPONSE_BODY","code":"server_error"}}'
            )
            self.send_response(503)
            self.send_header("x-request-id", "req-http-failure-fixture")
            self.send_header("cf-ray", "fixture-ray")
            self.send_header("content-length", str(len(body)))
            self.send_header("content-type", "application/json")
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            return
        # The shared SSE harness closes the connection to terminate each stream.
        self.close_connection = True
        return super().do_POST()


def read_ledger(binary, config, thread_id):
    environment = os.environ | config.env
    result = subprocess.run(
        [
            str(binary),
            "debug",
            "network-errors",
            "--include-context",
            "--thread-id",
            thread_id,
            "--limit",
            "100",
        ],
        env=environment,
        cwd=config.cwd,
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    return json.loads(result.stdout)["data"]


def verify(binary):
    with tempfile.TemporaryDirectory(prefix="codex-network-proof-") as temporary:
        harness = AppServerHarness(Path(temporary))
        harness.responses._server.RequestHandlerClass = IncidentHandler
        harness.responses.healthy_websocket = False
        harness.responses.websocket_requests = 0
        harness.responses.fail_next_http = False
        with harness:
            config_path = harness.codex_home / "config.toml"
            original = config_path.read_text().replace(
                "stream_max_retries = 0",
                "stream_max_retries = 1\nstream_idle_timeout_ms = 3000",
            )
            bounded = "\n[features]\nunbounded_connection_retries = false\n"
            config_path.write_text(
                original.replace("stream_max_retries = 1", "stream_max_retries = 2")
                + "supports_websockets = true\n"
                + bounded
            )
            config = CodexConfig(
                codex_bin=str(binary),
                cwd=str(harness.workspace),
                env={
                    "CODEX_HOME": str(harness.codex_home),
                    "CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG": "1",
                    "RUST_LOG": "error",
                },
            )
            harness.responses.enqueue_assistant_message("HTTP_RECOVERED")
            with Codex(config=config) as client:
                thread = client.thread_start(
                    ephemeral=True,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.read_only,
                )
                events = list(thread.turn("PRIVATE_TEST_PROMPT").stream())
                finished = next(
                    event.payload.turn
                    for event in events
                    if event.method == "turn/completed"
                )
                assert finished.status.value == "completed", finished.error
                errors = [
                    event.payload.error for event in events if event.method == "error"
                ]
                assert any(
                    "1008" in (error.additional_details or "") for error in errors
                ), "close details did not reach the desktop protocol"
                websocket_thread = thread.id
            websocket_rows = read_ledger(binary, config, websocket_thread)
            assert (harness.codex_home / "network_diagnostics_1.sqlite").is_file()
            assert any(
                row["event"] == "websocket_provider_error"
                and row["details"].get("error_code") == "fixture_overloaded"
                for row in websocket_rows
            )
            closes = [
                row for row in websocket_rows if row["event"] == "websocket_close"
            ]
            assert closes and all(
                row["details"]["close_code"] == 1008 for row in closes
            )
            assert any(row["turnId"] for row in closes)
            assert all(
                row["details"]["last_event"] == "response.created" for row in closes
            )
            assert any(row["event"] == "fallback_to_http" for row in websocket_rows)
            assert any(
                row["details"].get("kind") == "response.completed"
                for row in websocket_rows
            )
            print(
                json.dumps(
                    {
                        "case": "websocket_close_fallback_and_desktop_details",
                        "passed": True,
                    }
                ),
                flush=True,
            )

            # Successful completion followed by a normal socket close is not an incident.
            harness.responses.healthy_websocket = True
            with Codex(config=config) as client:
                thread = client.thread_start(
                    ephemeral=True,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.read_only,
                )
                assert thread.run("healthy fixture").final_response == "WS_OK"
                healthy_thread = thread.id
            assert not any(
                row["event"] == "websocket_close"
                for row in read_ledger(binary, config, healthy_thread)
            )
            print(
                json.dumps({"case": "normal_websocket_completion", "passed": True}),
                flush=True,
            )

            config_path.write_text(original + bounded)
            harness.responses.fail_next_http = True
            harness.responses.enqueue_sse(
                sse([ev_response_created("truncated-http-stream")])
            )
            with Codex(config=config) as client:
                thread = client.thread_start(
                    ephemeral=True,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.read_only,
                )
                events = list(thread.turn("interrupted HTTP fixture").stream())
                failed = next(
                    event.payload.turn
                    for event in events
                    if event.method == "turn/completed"
                )
                assert failed.status.value == "failed"
                harness.responses.enqueue_assistant_message("NEXT_TURN_RECOVERED")
                assert (
                    thread.run("recovery fixture").final_response
                    == "NEXT_TURN_RECOVERED"
                )
                http_thread = thread.id
            http_rows = read_ledger(binary, config, http_thread)
            assert any(
                row["details"].get("http_status") == 503
                and row["details"].get("request_id") == "req-http-failure-fixture"
                for row in http_rows
            )
            assert any(
                row["details"].get("error_code") == "server_error" for row in http_rows
            )
            assert any(
                row["event"] == "codex.sse_event" and row["details"].get("failed")
                for row in http_rows
            )
            assert any(
                row["event"] == "http_stream_failed"
                and row["details"].get("kind") == "closed_before_completion"
                and row["turnId"]
                for row in http_rows
            )
            assert any(
                row["details"].get("kind") == "response.completed" for row in http_rows
            )
            assert read_ledger(binary, config, websocket_thread) == websocket_rows
            print(
                json.dumps(
                    {
                        "case": "http_error_stream_interruption_and_recovery",
                        "passed": True,
                    }
                ),
                flush=True,
            )
            harness.responses.close()
            with Codex(config=config) as client:
                thread = client.thread_start(
                    ephemeral=True,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.read_only,
                )
                events = list(thread.turn("connection refused fixture").stream())
                failed = next(
                    event.payload.turn
                    for event in events
                    if event.method == "turn/completed"
                )
                assert failed.status.value == "failed"
                refused_thread = thread.id
            refused_rows = read_ledger(binary, config, refused_thread)
            assert any(
                row["event"] == "http_transport_failure"
                and row["details"].get("origin") == harness.responses.url
                for row in refused_rows
            )
            assert any(
                "connection refused" in row["details"].get("error", "").lower()
                for row in refused_rows
            ), "transport cause chain was lost"
            retained = json.dumps(websocket_rows + http_rows + refused_rows)
            for excluded in (
                "synthetic-secret",
                "PRIVATE_TEST_PROMPT",
                "PRIVATE_RESPONSE_BODY",
            ):
                assert excluded not in retained, (
                    f"private content entered the ledger: {excluded}"
                )
            return {
                "websocket_close_records": len(closes),
                "http_records": len(http_rows),
                "desktop_close_details": True,
                "fallback_and_recovery": True,
                "normal_close_not_flagged": True,
                "retained_after_process_restart": True,
                "connection_failure_cause_retained": True,
                "credentials_and_payloads_excluded": True,
            }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    report = verify(args.binary.resolve(strict=True))
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
