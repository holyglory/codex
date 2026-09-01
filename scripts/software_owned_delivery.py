#!/usr/bin/env python3
"""Plan, apply, or roll back the canonical CodexMulti VPS release."""

import argparse
import json
from pathlib import Path
import sys

from software_owned_delivery_lib import DeliveryError
from software_owned_delivery_lib import DeliveryWorkflow
from software_owned_delivery_lib import PRODUCTION_TARGET_NAMES
from software_owned_delivery_lib import production_config


class StoreOnceAction(argparse.Action):
    def __call__(
        self,
        parser: argparse.ArgumentParser,
        namespace: argparse.Namespace,
        values: str,
        option_string: str | None = None,
    ) -> None:
        if getattr(namespace, self.dest, None) is not None:
            parser.error(f"{option_string} may be specified only once")
        setattr(namespace, self.dest, values)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser(
        "run",
        help="plan or apply one canonical local-account release",
    )
    run.add_argument(
        "--user",
        choices=PRODUCTION_TARGET_NAMES,
        required=True,
        action=StoreOnceAction,
    )
    run.add_argument("--mode", choices=("plan", "apply"), default="plan")
    run.add_argument("--artifact", required=True, type=Path)
    run.add_argument("--checksum-manifest", required=True, type=Path)
    run.add_argument("--plan-fingerprint")
    run.add_argument("--confirm")
    rollback = commands.add_parser(
        "rollback",
        help="restore launchers from durable evidence",
    )
    rollback.add_argument(
        "--user",
        choices=PRODUCTION_TARGET_NAMES,
        required=True,
        action=StoreOnceAction,
    )
    rollback.add_argument("--deployment-id", required=True)
    rollback.add_argument("--confirm", required=True)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    workflow = DeliveryWorkflow(production_config(args.user))
    try:
        if args.command == "run":
            if args.mode == "plan":
                if args.plan_fingerprint is not None or args.confirm is not None:
                    raise DeliveryError(
                        "argument_invalid",
                        "plan mode does not accept apply authorization",
                    )
                result = workflow.plan(
                    args.artifact,
                    args.checksum_manifest,
                ).public_payload()
            else:
                if args.plan_fingerprint is None or args.confirm is None:
                    raise DeliveryError(
                        "argument_invalid",
                        "apply mode requires a plan fingerprint and confirmation",
                    )
                result = workflow.apply(
                    args.artifact,
                    args.checksum_manifest,
                    args.plan_fingerprint,
                    args.confirm,
                )
        elif args.command == "rollback":
            result = workflow.rollback(args.deployment_id, args.confirm)
        else:
            raise AssertionError("unreachable command")
    except DeliveryError as error:
        print(
            json.dumps(
                {
                    "ok": False,
                    "code": error.code,
                    "message": error.message,
                },
                sort_keys=True,
            ),
            file=sys.stderr,
        )
        return 2
    except Exception:
        print(
            json.dumps(
                {
                    "ok": False,
                    "code": "unexpected_failure",
                    "message": (
                        "the delivery workflow failed without exposing internal data"
                    ),
                },
                sort_keys=True,
            ),
            file=sys.stderr,
        )
        return 2
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
