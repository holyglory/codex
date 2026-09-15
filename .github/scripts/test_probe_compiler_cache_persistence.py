import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

from compiler_cache_write_diagnostics import cache_write_diagnostics
from compiler_cache_write_diagnostics import classify_write_error
from install_retrying_sccache import cached_binary
from compiler_cache_health import cache_summary
from compiler_cache_health import health_failures
from probe_compiler_cache_persistence import verify_summary


class CompilerCacheProofTests(unittest.TestCase):
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
            "errors": 0,
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
            "errors": 0,
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
            "errors": 0,
            "pending_writes": 1,
        }
        self.assertEqual(health_failures(summary), ["pending_writes=1"])
        summary.update(writes=1, pending_writes=0)
        self.assertEqual(health_failures(summary), [])
        summary.update(rust_hits=0, rust_misses=0)
        self.assertEqual(
            health_failures(summary), ["No Rust compiler-cache activity was observed"]
        )


if __name__ == "__main__":
    unittest.main()
