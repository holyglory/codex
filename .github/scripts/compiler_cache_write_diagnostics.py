"""Keep only allowlisted cache-error classifications, never raw service errors."""

from collections import Counter
from contextlib import contextmanager
import os
from pathlib import Path
import re
import threading


def classify_write_error(message: str) -> set[str]:
    result = {
        f"http_{status}"
        for status in re.findall(
            r"(?:status(?:_code)?\D{0,8}|HTTP(?:/\d\.\d)?\s+)([1-5]\d{2})",
            message,
            re.IGNORECASE,
        )
    }
    lowered = message.lower()
    for category, phrases in {
        "rate_limited": ("rate limit", "ratelimit", "too many requests"),
        "quota": ("quota", "storage limit", "cache size limit"),
        "already_exists": ("already exists", "alreadyexists"),
        "permission_denied": (
            "permission denied",
            "permissiondenied",
            "not authorized",
        ),
        "timed_out": ("timed out", "timeout"),
        "retry_scheduled": ("will retry",),
    }.items():
        if any(phrase in lowered for phrase in phrases):
            result.add(category)
    return result or {"unclassified"}


@contextmanager
def cache_write_diagnostics(root: Path, inherited: dict[str, str]):
    # Service errors can contain signed URLs. A FIFO carries them directly to
    # this allowlist; raw messages are never written to a file or job output.
    pipe = root / "cache-write-diagnostics.pipe"
    os.mkfifo(pipe, 0o600)
    reader = os.open(pipe, os.O_RDONLY | os.O_NONBLOCK)
    anchor = os.open(pipe, os.O_WRONLY | os.O_NONBLOCK)
    os.set_blocking(reader, True)
    counters = Counter()
    sentinel = b"CODEX_CACHE_DIAGNOSTICS_DONE\n"

    def collect():
        with os.fdopen(reader, "rb") as source:
            truncated = False
            while chunk := source.readline(16_385):
                if chunk == sentinel:
                    return
                if len(chunk) > 16_384 or truncated:
                    if not truncated:
                        counters["oversized_message"] += 1
                    truncated = not chunk.endswith(b"\n")
                    continue
                if not chunk.strip():
                    continue
                counters["messages"] += 1
                counters.update(classify_write_error(chunk.decode(errors="replace")))

    worker = threading.Thread(
        target=collect, name="cache-write-diagnostics", daemon=True
    )
    worker.start()
    environment = dict(
        inherited,
        SCCACHE_LOG="sccache::server=debug,sccache::cache::gha=warn/(Error executing cache write|will retry)",
        SCCACHE_ERROR_LOG=str(pipe),
    )
    try:
        yield environment, counters
    finally:
        os.write(anchor, b"\n" + sentinel)
        worker.join(timeout=5)
        os.close(anchor)
        pipe.unlink()
        if worker.is_alive():
            raise RuntimeError("Cache diagnostics did not finish")
