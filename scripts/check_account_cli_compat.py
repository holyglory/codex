#!/usr/bin/env python3
"""Run only through the governed parent check; no live Codex home is used.

Capture a reference without accepting source drift:
  python scripts/check_account_cli_compat.py capture --binary /path/to/reference/codex \
    --source-root /home/CodexMulti --output /retained/account-reference.json
Compare an identified candidate against that exact artifact:
  python scripts/check_account_cli_compat.py compare --binary /path/to/candidate/codex \
    --reference /retained/account-reference.json --output /retained/account-candidate.json
Use matrix, --group, and --case for bounded governed graph cells. Outputs are never overwritten.
Successful/cancelled add output is checked by account_cmd/add_tests.rs through its injected authorizer.
Actual OAuth protocol tests remain in login; this runner never starts real authorization.
"""

from account_cli_compat.runner import main

if __name__ == "__main__":
    raise SystemExit(main())
