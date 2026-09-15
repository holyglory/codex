import unittest

from probe_compiler_cache_persistence import cache_summary, verify_summary


class CompilerCacheProofTests(unittest.TestCase):
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
        }
        self.assertEqual(
            verify_summary(summary, "consume", 512),
            [
                "The GitHub Actions cache backend was not used",
            ],
        )


if __name__ == "__main__":
    unittest.main()
