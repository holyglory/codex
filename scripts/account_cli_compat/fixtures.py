"""Synthetic profiles based on login/profile/router_tests and backend-client fixtures."""

import base64
import copy
import hashlib
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

IDS = tuple(f"acct_{value:032x}" for value in range(1, 7))
LONG_ALIAS = "long-account-alias-" + "x" * 45
RETIRED = b"retired usage history: this is deliberately not a SQLite database\n"

# These inputs belong to the separate installed-binary credit comparison. None
# of the backend modes used by the frozen 190-row matrix changes.
CREDIT_USAGE_PAYLOADS = {
    "credits.balance": {
        "credits": {"has_credits": True, "unlimited": False, "balance": "9.99"},
    },
    "credits.no_credits": {
        "credits": {"has_credits": False, "unlimited": False, "balance": "999"},
    },
    "credits.unlimited": {
        "credits": {"has_credits": True, "unlimited": True, "balance": "0"},
    },
    "credits.missing_balance": {
        "credits": {"has_credits": True, "unlimited": False},
    },
    "credits.null_balance": {
        "credits": {"has_credits": True, "unlimited": False, "balance": None},
    },
    "credits.blank_balance": {
        "credits": {"has_credits": True, "unlimited": False, "balance": "   "},
    },
    "credits.control_balance": {
        "credits": {"has_credits": True, "unlimited": False, "balance": "12\n34"},
    },
    "credits.overlong_balance": {
        "credits": {"has_credits": True, "unlimited": False, "balance": "7" * 129},
    },
    "credits.auxiliary_only": {
        "additional_rate_limits": [
            {
                "limit_name": "synthetic auxiliary quota",
                "metered_feature": "synthetic_auxiliary",
                "rate_limit": {
                    "allowed": True,
                    "limit_reached": False,
                    "primary_window": {
                        "used_percent": 95,
                        "limit_window_seconds": 900,
                        "reset_after_seconds": 0,
                        "reset_at": 0,
                    },
                },
                # Auxiliary credits are not modeled by the current API. This
                # wire probe lets the installed binary establish its behavior.
                "credits": {"has_credits": True, "unlimited": False, "balance": "999"},
            }
        ],
    },
}


def write_private(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.write_bytes(content)
    path.chmod(0o600)


def auth(alias: str, *, api_key: bool = False, legacy: bool = False) -> dict:
    if api_key:
        return {"OPENAI_API_KEY": "synthetic-account-golden-key"}
    claims = {
        "email": f"{alias}@example.invalid",
        "exp": 4102444800,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "plus",
            "chatgpt_account_id": f"{alias}-workspace",
            "chatgpt_user_id": f"{alias}-user",
        },
    }

    def encode(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")

    token = f"{encode({'alg': 'none', 'typ': 'JWT'})}.{encode(claims)}.c2lnbmF0dXJl"
    result = {
        "OPENAI_API_KEY": None,
        "tokens": {
            "id_token": token,
            "access_token": f"fixture-{alias}-access",
            "refresh_token": "synthetic-refresh",
            "account_id": f"{alias}-workspace",
        },
        "last_refresh": "2100-01-01T00:00:00Z",
    }
    if not legacy:
        result["auth_mode"] = "chatgpt"
    return result


def seed(codex_home: Path, fixture: str, backend_url: str) -> dict | None:
    codex_home.mkdir(mode=0o700)
    config = f'cli_auth_credentials_store = "file"\nchatgpt_base_url = "{backend_url}/backend-api"\n'
    config += (
        "suppress_unstable_features_warning = true\n[analytics]\nenabled = false\n"
    )
    if fixture == "api_policy":
        config = 'forced_login_method = "api"\n' + config
    elif fixture == "ephemeral":
        config = config.replace('= "file"', '= "ephemeral"', 1)
    elif fixture == "bad_config":
        config = "this is not valid TOML [\n"
    write_private(codex_home / "config.toml", config.encode())
    write_private(codex_home / "usage/usage.sqlite3", RETIRED)
    write_private(
        codex_home / "sessions/retired.jsonl",
        b"retired rollout must remain untouched\n",
    )
    accounts = []
    specs = [
        ("alpha", 1000, True, "Primary"),
        ("beta", 1000, True, None),
        ("guest", 900, True, None),
        ("disabled", 500, False, "offline"),
        ("zeta", 10, True, None),
        (LONG_ALIAS, 10, True, "東京 team"),
    ]
    for index, (alias, priority, enabled, note) in enumerate(specs):
        account = {
            "id": IDS[index],
            "alias": alias,
            "authMode": "apikey" if alias == "zeta" else "chatgpt",
            "email": f"{alias}@example.invalid",
            "planType": "plus",
            "enabled": enabled,
            "priority": priority,
            "createdAt": "2020-01-02T03:04:05Z",
            "lastUsedAt": None,
            "note": note,
            "serviceAccountId": None,
            "serviceWorkspaceId": None,
        }
        accounts.append(account)
    if fixture in {"empty", "missing"}:
        accounts = []
    if fixture == "single":
        accounts = accounts[:1]
    if fixture == "ambiguous":
        accounts[1]["alias"] = IDS[0]
    (codex_home / "accounts").mkdir(mode=0o700)
    for index, account in enumerate(accounts):
        profile = codex_home / "accounts" / account["id"]
        profile.mkdir(parents=True, mode=0o700)
        if account["alias"] != "guest" and fixture != "no_auth":
            payload = auth(
                specs[index][0],
                api_key=account["authMode"] == "apikey",
                legacy=index == 1,
            )
            if fixture == "bad_mode" and index == 0:
                payload = auth("alpha", api_key=True)
            write_private(
                profile / "auth.json", json.dumps(payload, indent=2).encode() + b"\n"
            )
    registry = {
        "version": 1,
        "generation": 41,
        "defaultAccountId": accounts[0]["id"]
        if accounts and fixture != "no_default"
        else None,
        "autoSelection": {"enabled": fixture == "auto_on", "policy": "priority"},
        "accounts": accounts,
    }
    if fixture == "all_equal":
        for account in accounts:
            account["priority"] = 10
    if fixture == "missing":
        return None
    write_private(
        codex_home / "accounts/index.json",
        json.dumps(registry, indent=2).encode() + b"\n",
    )
    return copy.deepcopy(registry)


def fingerprints(codex_home: Path) -> dict:
    paths = list((codex_home / "accounts").glob("*/auth.json"))
    paths += [codex_home / "usage/usage.sqlite3", codex_home / "sessions/retired.jsonl"]
    return {
        path.relative_to(codex_home).as_posix(): hashlib.sha256(
            path.read_bytes()
        ).hexdigest()
        for path in paths
        if path.is_file()
    }


class Backend:
    """Loopback-only service; unexpected traffic is retained and fails contract validation."""

    def __init__(self, mode: str):
        self.mode, self.consumed, self.requests, self.unexpected = mode, False, [], []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                self.respond()

            def do_POST(self):
                self.respond()

            def respond(self):
                body = self.rfile.read(
                    int(self.headers.get("Content-Length", "0"))
                ).decode()
                authorization = self.headers.get("Authorization", "")
                profile = authorization.removeprefix("Bearer fixture-").removesuffix(
                    "-access"
                )
                if profile not in {"alpha", "beta", "disabled", LONG_ALIAS}:
                    profile = "<unrecognized>"
                    fixture.unexpected.append("unrecognized fixture authorization")
                fixture.requests.append(
                    {
                        "method": self.command,
                        "path": self.path,
                        "profile": profile,
                        "body": body,
                    }
                )
                status, payload = 200, {}
                prefix = "/backend-api/wham/"
                if self.command == "GET" and self.path == prefix + "usage":
                    percent = {"alpha": 25, "beta": 80}.get(profile, 10)
                    payload = {
                        "plan_type": "plus",
                        "rate_limit": {
                            "allowed": True,
                            "limit_reached": False,
                            "primary_window": {
                                "used_percent": percent,
                                "limit_window_seconds": 18000,
                                "reset_after_seconds": 0,
                                "reset_at": 0,
                            },
                        },
                        "rate_limit_reset_credits": {
                            "available_count": int(not fixture.consumed)
                        },
                    }
                    payload.update(copy.deepcopy(CREDIT_USAGE_PAYLOADS.get(fixture.mode, {})))
                    if fixture.mode == "request_failed":
                        status, payload = 400, {"error": "synthetic usage failure"}
                elif (
                    self.command == "GET"
                    and self.path == prefix + "rate-limit-reset-credits"
                ):
                    count = int(not fixture.consumed and fixture.mode != "no_credit")
                    payload = {
                        "available_count": count,
                        "credits": [
                            {
                                "id": "credit-a",
                                "reset_type": "codex_rate_limits",
                                "status": "available",
                                "granted_at": "2020-01-01T00:00:00Z",
                                "expires_at": None,
                            }
                        ]
                        if count
                        else [],
                    }
                elif (
                    self.command == "POST"
                    and self.path == prefix + "rate-limit-reset-credits/consume"
                ):
                    if fixture.mode in {"reject", "uncertain"}:
                        status, payload = (
                            (400 if fixture.mode == "reject" else 500),
                            {"error": "synthetic rejection"},
                        )
                    else:
                        code = (
                            fixture.mode
                            if fixture.mode
                            in {
                                "nothing_to_reset",
                                "already_redeemed",
                                "no_credit_response",
                            }
                            else "reset"
                        )
                        code = "no_credit" if code == "no_credit_response" else code
                        payload = {
                            "code": code,
                            "windows_reset": 2 if code == "reset" else 0,
                        }
                        fixture.consumed = code != "nothing_to_reset"
                else:
                    fixture.unexpected.append(f"{self.command} {self.path}")
                    status, payload = 404, {"error": "unexpected fixture endpoint"}
                encoded = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
