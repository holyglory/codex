"""Named source expectations for account CLI golden captures."""

import copy
import json

ERRORS = {
    "unknownAccount": "account profile was not found",
    "ambiguousAccount": "account reference is ambiguous",
    "disabledAccount": "account profile is disabled",
    "notAuthenticated": "account profile is not authenticated",
    "generationConflict": "account registry changed concurrently",
    "confirmationRequired": "explicit confirmation is required; pass --yes",
    "duplicateAccount": "account profile is already registered",
    "rateLimitsUnavailable": "rate-limit information is unavailable",
    "noCredit": "no banked reset credit is available",
    "resetFailed": "banked reset could not be applied",
    "resetUncertain": "banked reset result is uncertain; retry with the same request id",
    "resetUnsupportedAuth": "banked resets require ChatGPT authentication",
    "invalidInput": "account command input is invalid",
    "credentialStoreFailure": "credential storage operation failed",
    "configurationFailure": "Codex configuration could not be loaded",
    "integrityFailure": "account profile integrity checks failed",
}

VIEW_KEYS = [
    "id",
    "alias",
    "authMode",
    "email",
    "planType",
    "enabled",
    "authenticated",
    "priority",
    "createdAt",
    "lastUsedAt",
    "note",
    "current",
]


def patched(registry: dict | None, changes: dict) -> dict | None:
    result = copy.deepcopy(registry)
    for path, value in changes.items():
        node = result
        parts = path.split("/")
        for part in parts[:-1]:
            node = node[int(part)] if isinstance(node, list) else node[part]
        key = int(parts[-1]) if isinstance(node, list) else parts[-1]
        if value == "<DELETE>":
            node.pop(key)
        else:
            node[key] = value
    return result


def expected_keys(arguments: tuple[str, ...]) -> list[str]:
    action = arguments[0]
    if action in {"current", "show"}:
        return ["schemaVersion", "generation", "account"]
    if (
        action == "list"
        or action == "priority"
        and (len(arguments) == 1 or arguments[1] == "list")
    ):
        return [
            "schemaVersion",
            "generation",
            "activeAccount",
            "priorityOrder",
            "accounts",
        ]
    if action == "priority" and arguments[1] == "set-all":
        return [
            "schemaVersion",
            "generation",
            "action",
            "changed",
            "changedCount",
            "priority",
            "accounts",
        ]
    return {
        "auto": [
            "schemaVersion",
            "generation",
            "enabled",
            "policy",
            "priorityOrder",
            "changed",
        ],
        "remove": [
            "schemaVersion",
            "generation",
            "removedAccount",
            "credentialsRemoved",
            "activeAccount",
        ],
        "doctor": ["schemaVersion", "healthy", "generation", "checks"],
        "limits": ["schemaVersion", "observedAt", "accounts"],
        "reset": [
            "schemaVersion",
            "account",
            "creditId",
            "requestId",
            "outcome",
            "windowsReset",
            "refreshedUsage",
            "refreshedBankedResets",
            "refreshError",
        ],
    }.get(action, ["schemaVersion", "generation", "action", "changed", "account"])


def human_contract(case, before, after, fingerprints_after):
    action = case.arguments[0] if case.arguments else ""
    if action == "doctor" and before:
        checks = [
            ("registryValid", "ok"),
            ("privatePermissions", "ok"),
            ("pendingProfileLogin", "ok"),
            ("activeAccountResolution", "ok"),
            ("credentialBackends", "error" if case.fixture == "bad_mode" else "ok"),
        ]
        return "".join(f"{code}: {status}\n" for code, status in checks)
    if case.exit_code != 0:
        return None
    if action == "auto":
        enabled = after["autoSelection"]["enabled"] if after else False
        return (
            f"Automatic account selection is {'on' if enabled else 'off'} "
            "(priority policy: higher numbers drain first and smaller numbers drain last; "
            "eligible: locally managed ChatGPT OAuth profiles only).\n"
        )
    if action == "remove":
        alias = case.arguments[1]
        return (
            f"Removed account {alias} and its stored credentials.\n"
            if case.remove_credentials
            else f"Removed account {alias}; no stored credentials were present.\n"
        )
    if case.arguments[:2] == ("priority", "set-all"):
        value = int(case.arguments[2])
        changed = sum(account["priority"] != value for account in before["accounts"])
        return (
            f"Set priority {value} for {len(before['accounts'])} account profile(s); {changed} changed. "
            "Higher numbers drain first; smaller numbers drain last.\n"
        )
    if action in {
        "current",
        "show",
        "rename",
        "edit",
        "use",
        "enable",
        "disable",
    } or case.arguments[:2] == ("priority", "set"):
        reference = (
            before["defaultAccountId"]
            if action == "current"
            else case.arguments[2]
            if action == "priority"
            else case.arguments[1]
        )
        original = next(
            account
            for account in before["accounts"]
            if reference in (account["id"], account["alias"])
        )
        account = next(
            account for account in after["accounts"] if account["id"] == original["id"]
        )
        if action not in {"current", "show"}:
            outcome = "updated" if case.patch else "was already unchanged"
            return f"Account {account['alias']} {outcome}.\n"
        authenticated = f"accounts/{account['id']}/auth.json" in fingerprints_after
        mode = "api-key" if account["authMode"] == "apikey" else account["authMode"]
        lines = [
            f"Account: {account['alias']}",
            f"Status: {'enabled' if account['enabled'] else 'disabled'}",
            f"Authenticated: {'yes' if authenticated else 'no'}",
            f"Authentication: {mode}",
            f"Priority: {account['priority']}",
        ]
        if account["email"] is not None:
            lines.append("Email: " + account["email"])
        if account["note"] is not None:
            lines.append("Note: " + account["note"])
        return "\n".join(lines) + "\n"
    return None


def validate(case, style, result, initial, expected_fingerprints) -> list[str]:
    failures = []
    if result["exitCode"] != case.exit_code:
        failures.append(
            f"source exit code {case.exit_code} required; got {result['exitCode']}"
        )
    after = (
        json.loads(result["registryAfter"])
        if result["registryAfter"] is not None
        else None
    )
    if after != patched(initial, case.patch):
        failures.append(
            "registry values/generation differ from the declared source mutation"
        )
    for account_id in case.remove_credentials:
        expected_fingerprints.pop(f"accounts/{account_id}/auth.json")
    if result["fingerprintsAfter"] != expected_fingerprints:
        failures.append("preserved credentials or retired history changed unexpectedly")
    if result["unexpectedRequests"]:
        failures.append(
            "backend received traffic outside the named source fixture contract"
        )
    if case.error:
        expected = (
            json.dumps(
                {
                    "schemaVersion": 1,
                    "error": {"code": case.error, "message": ERRORS[case.error]},
                },
                separators=(",", ":"),
            )
            + "\n"
            if style == "json"
            else f"Error: {ERRORS[case.error]}\n"
        )
        if result["stderr"] != expected:
            failures.append(
                "error text, JSON schema/order, or stderr stream differs from named source"
            )
    elif case.exit_code == 0 and result["stderr"]:
        failures.append("successful source command unexpectedly wrote stderr")
    if style == "human":
        expected_human = human_contract(
            case, initial, after, result["fingerprintsAfter"]
        )
        if expected_human is not None and result["stdout"] != expected_human:
            failures.append("human message wording/stream differs from named source")
    if style == "json" and result["stdout"] and case.exit_code != 2:
        payload = json.loads(result["stdout"])
        if (
            list(payload) != expected_keys(case.arguments)
            or payload["schemaVersion"] != 1
        ):
            failures.append("JSON field order/schema differs from named source")
        if "generation" in payload and payload["generation"] != (
            after["generation"] if after else 0
        ):
            failures.append("reported generation differs from the persisted registry")
        account_views = (
            [payload["account"]]
            if isinstance(payload.get("account"), dict)
            else payload.get("accounts", [])
            if case.arguments[0] in {"list", "priority"}
            else []
        )
        if case.arguments[:2] == ("priority", "set-all"):
            account_views = []
        for account in account_views:
            keys = VIEW_KEYS + (["limits"] if case.arguments[0] == "list" else [])
            if list(account) != keys:
                failures.append(
                    "account JSON field order/schema differs from named source"
                )
                break
            stored = next(
                (row for row in after["accounts"] if row["id"] == account["id"]), None
            )
            if stored is None:
                failures.append(
                    "reported account was not found in the persisted registry"
                )
                continue
            expected_view = {
                key: stored[key]
                for key in VIEW_KEYS
                if key not in {"authenticated", "current"}
            }
            expected_view["createdAt"] = stored["createdAt"].replace("Z", "+00:00")
            expected_view["authenticated"] = (
                f"accounts/{stored['id']}/auth.json" in result["fingerprintsAfter"]
            )
            expected_view["current"] = after["defaultAccountId"] == stored["id"]
            if {key: account[key] for key in VIEW_KEYS} != expected_view:
                failures.append(
                    "reported account metadata/authentication/default differs from persisted source state"
                )
        if case.arguments[0] in {"list", "priority"} and "priorityOrder" in payload:
            order = [
                (account["priority"], account["alias"], account["id"])
                for account in account_views
            ]
            if order != sorted(order, key=lambda item: (-item[0], item[1], item[2])):
                failures.append(
                    "JSON account sorting differs from higher-first source order"
                )
    if (
        style == "human"
        and case.arguments[0:1] in {("list",), ("priority",)}
        and initial
        and initial["accounts"]
    ):
        is_list = case.arguments[0] == "list"
        is_priority_list = len(case.arguments) == 1 or case.arguments[1] == "list"
        if is_list or is_priority_list:
            width = (
                max(5, max(len(account["alias"]) for account in initial["accounts"]))
                + 1
            )
            header = "ALIAS".ljust(width) + "\tPRIORITY\tNOTE"
            if is_list:
                header += "\tBANKED RESETS\tEXPIRY IN\tLIMITS\tRESET IN"
            if result["stdout"].splitlines()[0] != header:
                failures.append("human table columns/padding differ from named source")
            aliases = [
                line.split("\t", 1)[0].strip().removeprefix("*").strip()
                for line in result["stdout"].splitlines()[1:]
            ]
            expected_aliases = [
                account["alias"]
                for account in sorted(
                    initial["accounts"],
                    key=lambda account: (
                        account["priority"],
                        account["alias"],
                        account["id"],
                    ),
                )
            ]
            if aliases != expected_aliases:
                failures.append(
                    "human account sorting differs from lower-first source order"
                )
    return failures
