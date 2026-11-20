#!/usr/bin/env python3
"""Read content-free live account and writer ownership evidence without mutations."""

import hashlib
import json
import os
from pathlib import Path
import socket

from local_deploy_owner import peer
from local_deploy_rpc import Rpc


class ObservedRpc(Rpc):
    def __init__(self, endpoint):
        super().__init__(endpoint)
        self.account_notifications = []

    def receive(self):
        value = super().receive()
        if value.get("method") == "account/updated":
            self.account_notifications.append(
                {"authMode": value.get("params", {}).get("authMode")}
            )
        return value


def main():
    home = Path("/home/holyglory/.codex")
    endpoint = home / "app-server-control/app-server-control.sock"
    result = {
        "endpointPeer": peer(endpoint),
        "servers": [],
        "clients": [],
        "writerOwners": [],
    }
    binaries = {}
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if proc.stat().st_uid != os.getuid():
                continue
            argv = (proc / "cmdline").read_bytes().split(b"\0")
            if not argv or not argv[0]:
                continue
            executable = (proc / "exe").resolve(strict=True)
            if executable.name not in {"codex", "codex-app-server"}:
                continue
            server = b"app-server" in argv or executable.name == "codex-app-server"
            if executable not in binaries:
                with executable.open("rb") as source:
                    binaries[executable] = hashlib.file_digest(
                        source, "sha256"
                    ).hexdigest()
            fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
            environment = dict(
                part.split(b"=", 1)
                for part in (proc / "environ").read_bytes().split(b"\0")
                if b"=" in part
            )
            result["servers" if server else "clients"].append(
                {
                    "pid": int(proc.name),
                    "parentPid": int(fields[1]),
                    "startTicks": fields[19],
                    "executable": str(executable),
                    "sha256": binaries[executable],
                    "managed": b"--managed-daemon" in argv,
                    "unixListener": b"unix://" in argv,
                    "stdio": b"stdio://" in argv or b"--listen" not in argv,
                    "resume": b"resume" in argv,
                    "explicitRemote": b"--remote" in argv,
                    "noDaemon": b"--no-daemon" in argv,
                    "accountOverride": b"--account" in argv,
                    "configOverrideKeys": [
                        argv[index + 1].decode(errors="replace").split("=", 1)[0][:60]
                        for index, arg in enumerate(argv[:-1])
                        if arg in {b"-c", b"--config"}
                    ],
                    "knownFlags": [
                        flag.decode()
                        for flag in (
                            b"--profile",
                            b"--enable",
                            b"--disable",
                            b"--oss",
                            b"--strict-config",
                            b"--dangerously-bypass-hook-trust",
                            b"--dangerously-bypass-approvals-and-sandbox",
                            b"--full-auto",
                            b"--search",
                        )
                        if flag in argv
                    ],
                    "executorOverride": bool(environment.get(b"CODEX_EXEC_SERVER_URL")),
                    "defaultCodexHome": environment.get(
                        b"CODEX_HOME", str(home).encode()
                    )
                    == str(home).encode(),
                }
            )
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    locks = (Path("/proc") / "locks").read_text().splitlines()
    names = {
        "01a1a38a-36b6-76b0-920c-5b9717214e6f",
        "01a191d7-4fdd-7ab3-9a3c-3c3835e97dd8",
    }
    names.update(
        path.stem
        for path in (home / "thread-writer-locks").glob("*.lock")
        if not path.name.startswith(".")
    )
    for name in sorted(names)[:100]:
        path = home / "thread-writer-locks" / (name + ".lock")
        item = {"threadId": name, "lockFileExists": path.exists(), "holders": []}
        if path.exists():
            state = path.stat()
            for line in locks:
                parts = line.split()
                if len(parts) < 6 or parts[1] == "->":
                    continue
                device = parts[5].split(":")
                if len(device) == 3 and (
                    int(device[0], 16),
                    int(device[1], 16),
                    int(device[2]),
                ) == (os.major(state.st_dev), os.minor(state.st_dev), state.st_ino):
                    item["holders"].append(
                        {"pid": int(parts[4]), "type": parts[1], "mode": parts[3]}
                    )
        result["writerOwners"].append(item)
    for kind in ("standalone", "app-server-daemon"):
        path = home / "packages" / kind / "current"
        result[kind + "Selection"] = str(path.resolve()) if path.exists() else None
    if result["endpointPeer"]:
        rpc = ObservedRpc(endpoint)
        try:
            initialized = rpc.call(
                "initialize",
                {
                    "clientInfo": {"name": "reconnect_diagnosis", "version": "1"},
                    "capabilities": {"experimentalApi": True},
                },
            )
            rpc.send({"method": "initialized", "params": {}})
            result["initialize"] = {
                key: initialized.get(key)
                for key in ("userAgent", "multiAccount", "localUsageAccounting")
            }
            for method, params in (
                ("account/read", {"refreshToken": False}),
                ("accountProfile/list", {"limit": 25}),
                ("getAuthStatus", {"includeToken": False, "refreshToken": False}),
            ):
                try:
                    response = rpc.call(method, params)
                    if method == "account/read":
                        account = response.get("account")
                        result[method] = {
                            "accountPresent": account is not None,
                            "accountType": account.get("type")
                            if isinstance(account, dict)
                            else None,
                            "requiresOpenaiAuth": response.get("requiresOpenaiAuth"),
                        }
                    elif method == "accountProfile/list":
                        rows = response.get("data", [])
                        result[method] = {
                            "count": len(rows),
                            "authenticated": sum(
                                row.get("authenticated") is True for row in rows
                            ),
                            "active": sum(row.get("isActive") is True for row in rows),
                            "defaults": sum(
                                row.get("isDefault") is True for row in rows
                            ),
                            "hasMore": response.get("nextCursor") is not None,
                        }
                    else:
                        result[method] = {
                            "authMethod": response.get("authMethod"),
                            "requiresOpenaiAuth": response.get("requiresOpenaiAuth"),
                            "tokenReturned": bool(response.get("authToken")),
                        }
                except Exception as error:
                    result[method] = {
                        "errorType": type(error).__name__,
                        "error": str(error)[:150],
                    }
            rpc.connection.settimeout(1)
            try:
                for _ in range(10):
                    rpc.receive()
            except socket.timeout:
                pass
            result["accountNotifications"] = rpc.account_notifications
        finally:
            rpc.connection.close()
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
