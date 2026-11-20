#!/usr/bin/env python3
"""Check an original chat through the public API without returning chat content."""

import argparse
import json
from pathlib import Path
import time

from local_deploy_rpc import Rpc

parser = argparse.ArgumentParser()
parser.add_argument("thread_id")
parser.add_argument("--resume", action="store_true")
args = parser.parse_args()
rpc = Rpc(Path("/home/holyglory/.codex/app-server-control/app-server-control.sock"))
proof = {"threadId": args.thread_id, "resumeRequested": args.resume}
try:
    rpc.call(
        "initialize",
        {
            "clientInfo": {"name": "reconnect_thread_probe", "version": "1"},
            "capabilities": {"experimentalApi": True},
        },
    )
    rpc.send({"method": "initialized", "params": {}})
    for method, params in [
        ("thread/read", {"threadId": args.thread_id, "includeTurns": False})
    ] + (
        [("thread/resume", {"threadId": args.thread_id, "excludeTurns": True})]
        if args.resume
        else []
    ):
        started = time.monotonic()
        response = rpc.call(method, params)
        thread = response.get("thread", {})
        proof[method] = {
            "elapsedMs": round((time.monotonic() - started) * 1000),
            "sameThread": thread.get("id") == args.thread_id,
            "status": thread.get("status"),
            "returnedTurns": len(thread.get("turns", [])),
        }
    proof["result"] = "passed"
except Exception as error:
    proof["result"] = "failed"
    proof["errorType"] = type(error).__name__
    proof["error"] = str(error)[:150]
finally:
    rpc.connection.close()
print(json.dumps(proof, sort_keys=True))
raise SystemExit(0 if proof["result"] == "passed" else 1)
