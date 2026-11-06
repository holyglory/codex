"""Every row runs in both output modes, from a fresh private fixture at generation 41."""

from dataclasses import asdict, dataclass, field

from .fixtures import IDS, LONG_ALIAS


@dataclass(frozen=True)
class Case:
    name: str
    arguments: tuple[str, ...]
    fixture: str = "profiles"
    group: str = "read"
    backend: str = "usage"
    exit_code: int = 0
    error: str | None = None
    patch: dict = field(default_factory=dict)
    remove_credentials: tuple[str, ...] = ()
    observed_at: bool = False
    last_used_at: bool = False
    request_id: bool = False

    def description(self) -> dict:
        return asdict(self)


def cases() -> list[Case]:
    result = [
        Case("list.mixed", ("list",)),
        Case("list.no_credentials", ("list",), fixture="no_auth"),
        Case("priority.default", ("priority",)),
        Case("priority.list", ("priority", "list")),
        Case("current", ("current",)),
        Case("show.alias", ("show", "alpha")),
        Case("show.id", ("show", IDS[1])),
        Case("show.long_alias", ("show", LONG_ALIAS)),
        Case("show.disabled", ("show", "disabled")),
        Case("show.unauthenticated", ("show", "guest")),
        Case("show.legacy_credentials", ("show", "beta")),
        Case("doctor.healthy", ("doctor",)),
        Case("doctor.no_credentials", ("doctor",), fixture="no_auth"),
        Case("auto.default", ("auto",)),
        Case("auto.status", ("auto", "status")),
        Case("auto.status_on", ("auto", "status"), fixture="auto_on"),
        Case(
            "rename.changed",
            ("rename", "alpha", "renamed"),
            group="mutation",
            patch={"generation": 42, "accounts/0/alias": "renamed"},
        ),
        Case("rename.noop", ("rename", "alpha", "alpha"), group="mutation"),
        Case(
            "edit.priority",
            ("edit", "alpha", "--priority", "55"),
            group="mutation",
            patch={"generation": 42, "accounts/0/priority": 55},
        ),
        Case(
            "edit.note",
            ("edit", "alpha", "--note", "東京 operators"),
            group="mutation",
            patch={"generation": 42, "accounts/0/note": "東京 operators"},
        ),
        Case(
            "edit.clear_note",
            ("edit", "alpha", "--clear-note"),
            group="mutation",
            patch={"generation": 42, "accounts/0/note": None},
        ),
        Case(
            "edit.combined",
            ("edit", "alpha", "--priority", "55", "--clear-note"),
            group="mutation",
            patch={
                "generation": 42,
                "accounts/0/priority": 55,
                "accounts/0/note": None,
            },
        ),
        Case("edit.noop", ("edit", "alpha", "--note", "Primary"), group="mutation"),
        Case(
            "priority.set",
            ("priority", "set", "beta", "55"),
            group="mutation",
            patch={"generation": 42, "accounts/1/priority": 55},
        ),
        Case(
            "priority.set_noop", ("priority", "set", "beta", "1000"), group="mutation"
        ),
        Case(
            "priority.set_all",
            ("priority", "set-all", "7"),
            group="mutation",
            patch={
                "generation": 42,
                **{f"accounts/{index}/priority": 7 for index in range(6)},
            },
        ),
        Case(
            "priority.set_all_noop",
            ("priority", "set-all", "10"),
            fixture="all_equal",
            group="mutation",
        ),
        Case(
            "use.changed",
            ("use", "beta"),
            group="mutation",
            last_used_at=True,
            patch={
                "generation": 42,
                "defaultAccountId": IDS[1],
                "accounts/1/lastUsedAt": "<LAST_USED_AT>",
            },
        ),
        Case("use.noop", ("use", "alpha"), group="mutation"),
        Case(
            "enable.changed",
            ("enable", "disabled"),
            group="mutation",
            patch={"generation": 42, "accounts/3/enabled": True},
        ),
        Case("enable.noop", ("enable", "alpha"), group="mutation"),
        Case(
            "enable.selects_missing_default",
            ("enable", "disabled"),
            fixture="no_default",
            group="mutation",
            patch={
                "generation": 42,
                "accounts/3/enabled": True,
                "defaultAccountId": IDS[3],
            },
        ),
        Case(
            "disable.changed",
            ("disable", "beta"),
            group="mutation",
            patch={"generation": 42, "accounts/1/enabled": False},
        ),
        Case(
            "disable.current_fallback",
            ("disable", "alpha"),
            group="mutation",
            patch={
                "generation": 42,
                "accounts/0/enabled": False,
                "defaultAccountId": IDS[1],
            },
        ),
        Case("disable.noop", ("disable", "disabled"), group="mutation"),
        Case(
            "remove.confirmed",
            ("remove", "beta", "--yes"),
            group="mutation",
            patch={"generation": 42, "accounts/1": "<DELETE>"},
            remove_credentials=(IDS[1],),
        ),
        Case(
            "remove.last",
            ("remove", "alpha", "--yes"),
            fixture="single",
            group="mutation",
            patch={
                "generation": 42,
                "accounts/0": "<DELETE>",
                "defaultAccountId": None,
            },
            remove_credentials=(IDS[0],),
        ),
        Case(
            "remove.no_credentials",
            ("remove", "guest", "--yes"),
            group="mutation",
            patch={"generation": 42, "accounts/2": "<DELETE>"},
        ),
        Case(
            "auto.on",
            ("auto", "on"),
            group="mutation",
            patch={"generation": 42, "autoSelection/enabled": True},
        ),
        Case("auto.on_noop", ("auto", "on"), fixture="auto_on", group="mutation"),
        Case(
            "auto.off",
            ("auto", "off"),
            fixture="auto_on",
            group="mutation",
            patch={"generation": 42, "autoSelection/enabled": False},
        ),
        Case("auto.off_noop", ("auto", "off"), group="mutation"),
        Case("limits.default", ("limits",), group="backend", observed_at=True),
        Case("limits.alias", ("limits", "beta"), group="backend", observed_at=True),
        Case("limits.all", ("limits", "--all"), group="backend", observed_at=True),
        Case(
            "limits.failed",
            ("limits", "alpha"),
            group="backend",
            backend="request_failed",
            exit_code=18,
            error="rateLimitsUnavailable",
            observed_at=True,
        ),
        Case(
            "limits.unsupported",
            ("limits", "zeta"),
            group="errors",
            exit_code=18,
            error="rateLimitsUnavailable",
            observed_at=True,
        ),
        Case(
            "limits.no_credentials",
            ("limits", "--all"),
            fixture="no_auth",
            group="errors",
            exit_code=18,
            error="rateLimitsUnavailable",
            observed_at=True,
        ),
        Case(
            "reset.selected",
            ("reset", "alpha", "--credit-id", "credit-a"),
            group="backend",
            backend="reset",
            request_id=True,
        ),
        Case(
            "reset.automatic_credit",
            ("reset", "alpha"),
            group="backend",
            backend="reset",
            request_id=True,
        ),
        Case(
            "reset.replay",
            (
                "reset",
                "alpha",
                "--credit-id",
                "credit-a",
                "--request-id",
                "fixture-request",
            ),
            group="backend",
            backend="already_redeemed",
        ),
        Case(
            "reset.nothing",
            ("reset", "alpha"),
            group="backend",
            backend="nothing_to_reset",
            request_id=True,
        ),
        Case(
            "reset.no_credit",
            ("reset", "alpha"),
            group="backend",
            backend="no_credit",
            exit_code=25,
            error="noCredit",
        ),
        Case(
            "reset.rejected",
            ("reset", "alpha"),
            group="backend",
            backend="reject",
            exit_code=26,
            error="resetFailed",
            request_id=True,
        ),
        Case(
            "reset.uncertain",
            ("reset", "alpha"),
            group="backend",
            backend="uncertain",
            exit_code=27,
            error="resetUncertain",
            request_id=True,
        ),
        Case(
            "unknown",
            ("show", "missing"),
            group="errors",
            exit_code=10,
            error="unknownAccount",
        ),
        Case(
            "ambiguous",
            ("show", IDS[0]),
            fixture="ambiguous",
            group="errors",
            exit_code=11,
            error="ambiguousAccount",
        ),
        Case(
            "disabled",
            ("use", "disabled"),
            group="errors",
            exit_code=12,
            error="disabledAccount",
        ),
        Case(
            "unauthenticated",
            ("use", "guest"),
            group="errors",
            exit_code=13,
            error="notAuthenticated",
        ),
        Case(
            "remove.confirmation",
            ("remove", "beta"),
            group="errors",
            exit_code=16,
            error="confirmationRequired",
        ),
        Case(
            "edit.invalid_note",
            ("edit", "alpha", "--note", "bad\nnote"),
            group="errors",
            exit_code=16,
            error="invalidInput",
        ),
        Case(
            "rename.duplicate",
            ("rename", "alpha", "beta"),
            group="errors",
            exit_code=19,
            error="duplicateAccount",
        ),
        Case(
            "add.duplicate",
            ("add", "alpha"),
            group="errors",
            exit_code=19,
            error="duplicateAccount",
        ),
        Case(
            "add.policy",
            ("add", "new", "--device-auth"),
            fixture="api_policy",
            group="errors",
            exit_code=22,
            error="configurationFailure",
        ),
        Case(
            "reset.unsupported",
            ("reset", "zeta"),
            group="errors",
            exit_code=28,
            error="resetUnsupportedAuth",
        ),
        Case(
            "show.integrity",
            ("show", "alpha"),
            fixture="bad_mode",
            group="errors",
            exit_code=23,
            error="integrityFailure",
        ),
        Case(
            "doctor.integrity",
            ("doctor",),
            fixture="bad_mode",
            group="errors",
            exit_code=23,
            error="integrityFailure",
        ),
        Case(
            "credential_store",
            ("current",),
            fixture="ephemeral",
            group="errors",
            exit_code=20,
            error="credentialStoreFailure",
        ),
        Case(
            "configuration",
            ("current",),
            fixture="bad_config",
            group="errors",
            exit_code=22,
            error="configurationFailure",
        ),
    ]
    for fixture in ("empty", "missing"):
        for action in (("list",), ("priority", "list"), ("auto", "status")):
            result.append(
                Case(f"{fixture}.{'.'.join(action)}", action, fixture=fixture)
            )
        result.append(
            Case(
                f"{fixture}.current",
                ("current",),
                fixture=fixture,
                group="errors",
                exit_code=10,
                error="unknownAccount",
            )
        )
    for arguments in (
        ("rename", "alpha", "new"),
        ("edit", "alpha", "--priority", "5"),
        ("priority", "set", "alpha", "5"),
        ("priority", "set-all", "5"),
        ("use", "beta"),
        ("enable", "disabled"),
        ("disable", "beta"),
        ("remove", "beta", "--yes"),
        ("auto", "on"),
        ("auto", "off"),
    ):
        result.append(
            Case(
                "stale." + ".".join(arguments[:2]),
                (*arguments, "--expected-generation", "40"),
                group="errors",
                exit_code=15,
                error="generationConflict",
            )
        )
    for name, arguments in [
        ("missing_action", ()),
        ("unknown_action", ("missing-command",)),
        ("add.invalid_alias", ("add", "INVALID")),
        ("edit.no_change", ("edit", "alpha")),
        ("edit.conflicting_notes", ("edit", "alpha", "--note", "x", "--clear-note")),
        ("limits.conflict", ("limits", "alpha", "--all")),
        ("priority.negative", ("priority", "set", "alpha", "-1")),
        (
            "reset.request_without_credit",
            ("reset", "alpha", "--request-id", "fixture-request"),
        ),
    ]:
        result.append(Case("parser." + name, arguments, group="parser", exit_code=2))
    return result
