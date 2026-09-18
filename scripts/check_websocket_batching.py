"""Verify bounded context uploads through an isolated, real Codex app-server."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import random
import socket
import struct
import tempfile
import zlib

from app_server_harness import AppServerHarness, _ResponsesHandler
from app_server_harness import ev_assistant_message, ev_completed, ev_response_created
from check_network_diagnostics import read_ledger
from openai_codex import (
    ApprovalMode,
    Codex,
    CodexConfig,
    ImageInput,
    Sandbox,
    TextInput,
)

LIMIT = 4 * 1024 * 1024


def image_inputs(value):
    if isinstance(value, dict):
        if value.get("type") == "input_image":
            yield hashlib.sha256(value["image_url"].encode()).hexdigest()
        for child in value.values():
            yield from image_inputs(child)
    elif isinstance(value, list):
        for child in value:
            yield from image_inputs(child)


class BatchHandler(_ResponsesHandler):
    protocol_version = "HTTP/1.1"

    def frame(self, opcode, payload):
        length = len(payload)
        header = bytes([128 | opcode])
        if length < 126:
            header += bytes([length])
        elif length < 65536:
            header += b"\x7e" + struct.pack("!H", length)
        else:
            header += b"\x7f" + struct.pack("!Q", length)
        self.wfile.write(header + payload)
        self.wfile.flush()

    def event(self, event):
        self.frame(1, json.dumps(event).encode())

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
        self.end_headers()
        self.connection.settimeout(20)
        cache = {}
        mock = self.server.mock
        try:
            while True:
                header = self.rfile.read(2)
                if len(header) != 2 or header[0] & 15 == 8:
                    return
                opcode = header[0] & 15
                length = header[1] & 127
                if length == 126:
                    length = struct.unpack("!H", self.rfile.read(2))[0]
                elif length == 127:
                    length = struct.unpack("!Q", self.rfile.read(8))[0]
                mask = self.rfile.read(4) if header[1] & 128 else None
                if length > LIMIT:
                    mock.batch_oversized += 1
                    self.frame(8, struct.pack("!H", 1009))
                    return
                payload = self.rfile.read(length)
                if mask:
                    payload = bytes(v ^ mask[i % 4] for i, v in enumerate(payload))
                if opcode == 9:
                    self.frame(10, payload)
                    continue
                assert opcode == 1 and header[0] & 128
                request = json.loads(payload)
                previous = request.get("previous_response_id")
                if previous:
                    assert previous in cache, "request referred to unavailable context"
                context = cache.get(previous, []) + request["input"]
                warmup = request.get("generate") is False
                images = list(image_inputs(context))
                if warmup and images and mock.batch_reject_staging:
                    mock.batch_rejections += 1
                    self.frame(8, struct.pack("!H", 1009))
                    return
                index = len(mock.batch_requests)
                response_id = f"batch-response-{index}"
                mock.batch_requests.append(
                    {"bytes": length, "warmup": warmup, "images": images}
                )
                self.event(ev_response_created(response_id))
                if not warmup:
                    output = ev_assistant_message(f"message-{index}", "BATCH_OK")
                    self.event(output)
                    context = context + [output["item"]]
                cache[response_id] = context
                self.event(ev_completed(response_id))
        except (OSError, socket.timeout):
            return
        finally:
            self.close_connection = True

    def do_POST(self):
        self.server.mock.batch_http += 1
        self.close_connection = True
        return super().do_POST()


def fixture_image():
    def chunk(kind, data):
        return (
            struct.pack("!I", len(data))
            + kind
            + data
            + struct.pack("!I", zlib.crc32(kind + data))
        )

    pixels = random.Random(17).randbytes(1024 * 768 * 3)
    rows = b"".join(b"\0" + pixels[i : i + 3072] for i in range(0, len(pixels), 3072))
    png = b"\x89PNG\r\n\x1a\n" + chunk(
        b"IHDR", struct.pack("!IIBBBBB", 1024, 768, 8, 2, 0, 0, 0)
    )
    png += chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
    return "data:image/png;base64," + base64.b64encode(png).decode()


def verify(binary):
    with tempfile.TemporaryDirectory(prefix="codex-batching-proof-") as directory:
        harness = AppServerHarness(Path(directory))
        mock = harness.responses
        mock._server.RequestHandlerClass = BatchHandler
        mock.batch_requests = []
        mock.batch_reject_staging = False
        mock.batch_rejections = 0
        mock.batch_oversized = 0
        mock.batch_http = 0
        with harness:
            path = harness.codex_home / "config.toml"
            path.write_text(path.read_text() + "supports_websockets = true\n")
            config = CodexConfig(
                codex_bin=str(binary),
                cwd=str(harness.workspace),
                env={
                    "CODEX_HOME": str(harness.codex_home),
                    "CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG": "1",
                    "RUST_LOG": "error",
                },
            )
            url = fixture_image()
            expected_images = [hashlib.sha256(url.encode()).hexdigest()] * 3
            with Codex(config=config) as client:
                thread = client.thread_start(
                    approval_mode=ApprovalMode.deny_all, sandbox=Sandbox.read_only
                )
                for index in range(3):
                    assert (
                        thread.run(
                            [TextInput(f"image fixture {index}"), ImageInput(url)]
                        ).final_response
                        == "BATCH_OK"
                    )
                thread_id = thread.id
            before = len(mock.batch_requests)
            with Codex(config=config) as client:
                thread = client.thread_resume(thread_id)
                assert (
                    thread.run("resume complete image context").final_response
                    == "BATCH_OK"
                )
            restored = mock.batch_requests[before:]
            assert sum(not x["warmup"] for x in restored) == 1
            assert sum(x["warmup"] and bool(x["images"]) for x in restored) >= 2
            assert restored[-1]["images"] == expected_images
            assert mock.batch_http == 0 and mock.batch_oversized == 0
            rows = read_ledger(binary, config, thread_id)
            assert any(row["event"] == "websocket_context_batch" for row in rows)
            assert all(
                row["details"]["request_bytes"] <= LIMIT
                for row in rows
                if row["event"] == "websocket_request_size"
            )
            assert url not in json.dumps(rows)
            print(
                json.dumps(
                    {"case": "restart_preserves_batched_images", "passed": True}
                ),
                flush=True,
            )

            mock.batch_reject_staging = True
            mock.enqueue_assistant_message("HTTP_RECOVERED")
            with Codex(config=config) as client:
                thread = client.thread_resume(thread_id)
                assert (
                    thread.run("recover rejected upload").final_response
                    == "HTTP_RECOVERED"
                )
            assert mock.batch_http == 1 and mock.batch_rejections == 1
            assert (
                list(image_inputs(mock.requests()[-1].body_json())) == expected_images
            )
            print(
                json.dumps(
                    {"case": "rejected_staging_recovers_over_http", "passed": True}
                ),
                flush=True,
            )

            mock.batch_reject_staging = False
            mock.enqueue_assistant_message("LARGE_ITEM_RECOVERED")
            with Codex(config=config) as client:
                thread = client.thread_start(
                    ephemeral=True,
                    approval_mode=ApprovalMode.deny_all,
                    sandbox=Sandbox.read_only,
                )
                result = thread.run(
                    [TextInput("one indivisible item")] + [ImageInput(url)] * 3
                )
                assert result.final_response == "LARGE_ITEM_RECOVERED"
            assert mock.batch_http == 2 and mock.batch_oversized == 0
            assert (
                list(image_inputs(mock.requests()[-1].body_json())) == expected_images
            )
            return {
                "restart_context_preserved": True,
                "single_generation": True,
                "rejected_staging_http_recovery": True,
                "indivisible_item_http_recovery": True,
                "max_websocket_request_bytes": max(
                    x["bytes"] for x in mock.batch_requests
                ),
                "request_sizes_retained_without_payloads": True,
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
