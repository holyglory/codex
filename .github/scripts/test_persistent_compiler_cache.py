"""Exercise real compiler results, cache corruption and the approved service."""

from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
import uuid

from compiler_cache_health import cache_summary, wait_for_cache_writes


@contextmanager
def compiler_trial(endpoint):
    with tempfile.TemporaryDirectory(prefix="pc-", dir="/tmp") as temporary:
        root = Path(temporary)
        source = root / "proof.rs"
        source.write_text("pub fn answer() -> u64 { 42 }\n")
        output = root / "out"
        output.mkdir()
        binary = os.environ["SCCACHE_TEST_BINARY"]
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("SCCACHE_") and key != "RUSTC_WRAPPER"
        }
        environment.update(
            TMPDIR=str(root),
            TEMP=str(root),
            TMP=str(root),
            SCCACHE_PATH=binary,
            SCCACHE_SERVER_UDS=str(root / "server.sock"),
            SCCACHE_DIR=str(root / "unused-local-cache"),
            SCCACHE_IDLE_TIMEOUT="0",
            SCCACHE_BAZEL_REMOTE_CACHE_URL=endpoint,
        )
        compiler = subprocess.check_output(
            ["rustup", "which", "rustc"], env=environment, text=True
        ).strip()

        def compile_source():
            result = subprocess.run(
                [
                    binary,
                    compiler,
                    "--crate-name",
                    "cache_proof",
                    "--crate-type",
                    "rlib",
                    "--emit=dep-info,metadata,link",
                    "--out-dir",
                    str(output),
                    str(source),
                ],
                cwd=root,
                env=environment,
                check=False,
                capture_output=True,
                timeout=90,
            )
            if result.returncode:
                raise RuntimeError(result.stderr.decode(errors="replace")[-2000:])
            summary = cache_summary(wait_for_cache_writes(environment))
            return summary, hashlib.sha256(
                (output / "libcache_proof.rlib").read_bytes()
            ).hexdigest()

        try:
            yield root, source, output, environment, compile_source
        finally:
            subprocess.run(
                [binary, "--stop-server"],
                env=environment,
                capture_output=True,
                timeout=30,
            )


class PersistentCompilerCacheTests(unittest.TestCase):
    def test_real_service_reuses_after_daemon_restart_and_invalidates_changed_source(
        self,
    ):
        endpoint = "http://127.0.0.1:9095/codex-compiler-proof-" + uuid.uuid4().hex
        with compiler_trial(endpoint) as (
            root,
            source,
            output,
            environment,
            compile_source,
        ):
            first, original = compile_source()
            self.assertEqual(
                {
                    key: first[key]
                    for key in (
                        "backend",
                        "rust_hits",
                        "rust_misses",
                        "writes",
                        "write_errors",
                        "read_errors",
                        "pending_writes",
                    )
                },
                {
                    "backend": "bazel-http",
                    "rust_hits": 0,
                    "rust_misses": 1,
                    "writes": 1,
                    "write_errors": 0,
                    "read_errors": 0,
                    "pending_writes": 0,
                },
            )
            subprocess.run(
                [environment["SCCACHE_PATH"], "--stop-server"],
                env=environment,
                check=True,
                capture_output=True,
                timeout=30,
            )
            environment["SCCACHE_SERVER_UDS"] = str(root / "fresh.sock")
            shutil.rmtree(output)
            output.mkdir()
            second, restored = compile_source()
            self.assertEqual(
                (
                    second["rust_hits"],
                    second["rust_misses"],
                    second["writes"],
                    restored,
                ),
                (1, 0, 0, original),
            )
            source.write_text("pub fn answer() -> u64 { 43 }\n")
            third, changed = compile_source()
            self.assertEqual(
                (
                    third["rust_hits"],
                    third["rust_misses"],
                    third["writes"],
                    third["write_errors"],
                ),
                (1, 1, 1, 0),
            )
            self.assertNotEqual(changed, original)

    def test_temporary_write_failure_recovers_and_corrupt_blob_is_recompiled(self):
        state = {
            "objects": {},
            "write_failures": 2,
            "corrupt_once": False,
            "retried": 0,
        }

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_PUT(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                if state["write_failures"]:
                    state["write_failures"] -= 1
                    state["retried"] += 1
                    self.send_response(503)
                else:
                    state["objects"][self.path] = body
                    self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def do_GET(self):
                body = state["objects"].get(self.path)
                if body is None:
                    self.send_error(404)
                    return
                if "/cas/" in self.path and state["corrupt_once"]:
                    body = b"corrupt compiler output"
                    state["corrupt_once"] = False
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                with compiler_trial(
                    f"http://127.0.0.1:{server.server_port}/fixture"
                ) as (_, _, output, _, compile_source):
                    first, original = compile_source()
                    self.assertEqual(
                        (state["retried"], first["writes"], first["write_errors"]),
                        (2, 1, 0),
                    )
                    state["corrupt_once"] = True
                    shutil.rmtree(output)
                    output.mkdir()
                    second, recovered = compile_source()
                    self.assertEqual(
                        (second["read_errors"], second["rust_misses"], recovered),
                        (1, 2, original),
                    )
                    third, cached = compile_source()
                    self.assertEqual((third["rust_hits"], cached), (1, original))
            finally:
                server.shutdown()
                worker.join(timeout=5)

    def test_endpoint_cannot_expand_the_confirmed_cache_tunnel_boundary(self):
        with compiler_trial("http://private-fixture@127.0.0.1:9095/cache") as (
            _,
            _,
            _,
            environment,
            _,
        ):
            result = subprocess.run(
                [environment["SCCACHE_PATH"], "--start-server"],
                env=environment,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("approved loopback SSH tunnel", result.stdout + result.stderr)
            self.assertNotIn("private-fixture", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
