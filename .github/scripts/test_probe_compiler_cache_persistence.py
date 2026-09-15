import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

from compiler_cache_write_diagnostics import cache_write_diagnostics
from compiler_cache_write_diagnostics import classify_write_error
from install_retrying_sccache import cached_binary
from compiler_cache_health import cache_summary
from compiler_cache_health import health_failures
from probe_compiler_cache_persistence import verify_summary


class CompilerCacheProofTests(unittest.TestCase):
    @unittest.skipUnless(
        os.name == "posix" and os.environ.get("SCCACHE_TEST_BINARY"),
        "Requires the installed cache tool on a POSIX runner",
    )
    def test_corrupt_cache_read_is_counted_and_recompiled_without_corrupting_output(
        self,
    ):
        with tempfile.TemporaryDirectory(
            prefix="cr-", dir=os.environ.get("RUNNER_TEMP")
        ) as temporary:
            root = Path(temporary)
            source = root / "probe.c"
            source.write_text("int answer(void) { return 42; }\n")
            output = root / "probe.o"
            environment = {
                key: value
                for key, value in os.environ.items()
                if not key.startswith("SCCACHE_")
            }
            environment.update(
                SCCACHE_DIR=str(root / "cache"),
                SCCACHE_SERVER_UDS=str(root / "s.sock"),
                SCCACHE_IDLE_TIMEOUT="0",
            )
            binary = os.environ["SCCACHE_TEST_BINARY"]
            command = [binary, shutil.which("cc"), "-c", str(source), "-o", str(output)]
            try:
                subprocess.run(
                    command,
                    env=environment,
                    check=True,
                    capture_output=True,
                    timeout=30,
                )
                expected = output.read_bytes()
                deadline = time.monotonic() + 20
                while True:
                    stats = json.loads(
                        subprocess.check_output(
                            [binary, "--show-stats", "--stats-format=json"],
                            env=environment,
                            timeout=10,
                        )
                    )["stats"]
                    if stats["cache_writes"] >= 1:
                        break
                    if time.monotonic() >= deadline:
                        self.fail("The isolated cache write did not finish")
                    time.sleep(0.5)
                entries = [
                    path for path in (root / "cache").rglob("*") if path.is_file()
                ]
                self.assertTrue(entries)
                for path in entries:
                    path.write_bytes(b"invalid cache entry")
                output.unlink()
                subprocess.run(
                    command,
                    env=environment,
                    check=True,
                    capture_output=True,
                    timeout=30,
                )
                stats = json.loads(
                    subprocess.check_output(
                        [binary, "--show-stats", "--stats-format=json"],
                        env=environment,
                        timeout=10,
                    )
                )["stats"]
                self.assertGreaterEqual(stats["cache_read_errors"], 1)
                self.assertEqual(output.read_bytes(), expected)
            finally:
                subprocess.run(
                    [binary, "--stop-server"],
                    env=environment,
                    check=True,
                    capture_output=True,
                    timeout=20,
                )

    def test_error_classification_omits_service_urls_and_credentials(self):
        self.assertEqual(
            classify_write_error(
                "Unexpected at write: status_code: 429, rate limit exceeded; "
                "url=https://cache.example/object?sig=private-fixture; "
                "Authorization: Bearer private-fixture"
            ),
            {"http_429", "rate_limited"},
        )
        self.assertEqual(
            classify_write_error("Unknown private-fixture"), {"unclassified"}
        )
        self.assertEqual(
            classify_write_error(
                "GitHub compiler cache RateLimited; will retry after 2.3s"
            ),
            {"rate_limited", "retry_scheduled"},
        )

    def test_cached_tool_rejects_changed_binary_or_source_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / ("sccache.exe" if os.name == "nt" else "sccache")
            binary.write_bytes(b"compiled tool fixture")
            identity = {"source_commit": "source", "patch_sha256": "patch"}
            (root / "receipt.json").write_text(
                json.dumps(
                    {
                        **identity,
                        "binary_sha256": hashlib.sha256(
                            binary.read_bytes()
                        ).hexdigest(),
                    }
                )
            )
            self.assertEqual(cached_binary(root, identity), binary)
            self.assertIsNone(
                cached_binary(root, {**identity, "patch_sha256": "changed"})
            )
            binary.write_bytes(b"changed tool fixture")
            self.assertIsNone(cached_binary(root, identity))

    @unittest.skipUnless(hasattr(os, "mkfifo"), "POSIX cache diagnostic stream")
    def test_diagnostics_keep_only_allowlisted_metadata_across_writer_connections(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with cache_write_diagnostics(root, {}) as (environment, counters):
                for message in (
                    b"status_code: 429; rate limit; sig=private-fixture\n",
                    b"status: 403; permission denied; token=private-fixture\n",
                ):
                    with open(environment["SCCACHE_ERROR_LOG"], "wb") as stream:
                        stream.write(message)
            self.assertEqual(
                dict(counters),
                {
                    "messages": 2,
                    "http_429": 1,
                    "rate_limited": 1,
                    "http_403": 1,
                    "permission_denied": 1,
                },
            )
            self.assertNotIn("private-fixture", json.dumps(counters))
            self.assertEqual(list(root.iterdir()), [])

    def test_write_failures_are_detected_when_the_generic_error_counter_is_zero(self):
        summary = cache_summary(
            {
                "cache_location": "ghac, name: fixture, prefix: /sccache/",
                "version": "0.16.0",
                "stats": {
                    "cache_hits": {"counts": {"Rust": 636}},
                    "cache_misses": {"counts": {"Rust": 481}},
                    "cache_errors": {"counts": {}},
                    "cache_writes": 499,
                    "cache_write_errors": 275,
                    "cache_read_errors": 0,
                    "cache_timeouts": 0,
                    "compilations": 774,
                },
            }
        )
        self.assertEqual(
            verify_summary(summary, "populate", 512),
            [
                "write_errors=275",
                "Expected 512 successful cache writes",
            ],
        )
        summary.update(write_errors=0, writes=512)
        self.assertEqual(verify_summary(summary, "populate", 512), [])

    def test_fresh_runner_recompilation_does_not_count_as_reuse(self):
        summary = {
            "backend": "ghac",
            "rust_hits": 511,
            "rust_misses": 1,
            "writes": 1,
            "write_errors": 0,
            "read_errors": 0,
            "timeouts": 0,
            "compiler_errors": 0,
            "pending_writes": 0,
        }
        self.assertEqual(
            verify_summary(summary, "consume", 512),
            [
                "Expected 512 Rust cache hits without recompilation",
            ],
        )
        summary.update(rust_hits=512, rust_misses=0, writes=0)
        self.assertEqual(verify_summary(summary, "consume", 512), [])

    def test_local_fallback_cannot_prove_persistent_github_cache(self):
        summary = {
            "backend": "Local disk",
            "rust_hits": 512,
            "rust_misses": 0,
            "writes": 0,
            "write_errors": 0,
            "read_errors": 0,
            "timeouts": 0,
            "compiler_errors": 0,
            "pending_writes": 0,
        }
        self.assertEqual(
            verify_summary(summary, "consume", 512),
            [
                "The GitHub Actions cache backend was not used",
            ],
        )

    def test_unfinished_cache_writes_cannot_be_reported_as_healthy(self):
        summary = {
            "backend": "ghac",
            "rust_hits": 1,
            "rust_misses": 1,
            "writes": 0,
            "write_errors": 0,
            "read_errors": 0,
            "timeouts": 0,
            "compiler_errors": 0,
            "pending_writes": 1,
        }
        self.assertEqual(health_failures(summary), ["pending_writes=1"])
        summary.update(writes=1, pending_writes=0)
        self.assertEqual(health_failures(summary), [])
        summary.update(rust_hits=0, rust_misses=0)
        self.assertEqual(
            health_failures(summary), ["No Rust compiler-cache activity was observed"]
        )

    def test_only_successful_cache_misses_require_persistent_writes(self):
        summary = cache_summary(
            {
                "cache_location": "ghac, name: fixture, prefix: /sccache/",
                "version": "0.16.0",
                "stats": {
                    "cache_hits": {"counts": {"Rust": 2}},
                    "cache_misses": {"counts": {"Rust": 4, "C/C++": 1}},
                    "cache_errors": {"counts": {}},
                    "compilations": 10,
                    "non_cacheable_compilations": 2,
                    "compile_fails": 3,
                    "cache_writes": 5,
                    "cache_write_errors": 0,
                    "cache_read_errors": 0,
                    "cache_timeouts": 0,
                },
            }
        )
        self.assertEqual(
            summary,
            {
                "backend": "ghac",
                "version": "0.16.0",
                "rust_hits": 2,
                "rust_misses": 4,
                "writes": 5,
                "write_errors": 0,
                "read_errors": 0,
                "timeouts": 0,
                "compiler_errors": 0,
                "pending_writes": 0,
            },
        )
        self.assertEqual(health_failures(summary), [])
        summary.update(compiler_errors=8)
        self.assertEqual(health_failures(summary), [])
        summary.update(read_errors=1)
        self.assertEqual(health_failures(summary), ["read_errors=1"])


if __name__ == "__main__":
    unittest.main()
