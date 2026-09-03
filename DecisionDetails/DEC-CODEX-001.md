# DEC-CODEX-001: Direct upstream-compatible multi-account fork

## Evidence and context

The approved architecture in `HANDOVER.md` requires the executable to remain
`codex`, preserves existing CLI and app-server behavior, and adds independent
credential profiles, account selection, safe automatic failover, migration,
and additive enhanced-client interfaces.

Official OpenAI documentation describes app-server as the protocol used for
authentication, conversation history, approvals, and streamed events, and
identifies the implementation as open source:
<https://learn.chatgpt.com/docs/app-server>.

## Options considered

- A shell wrapper around the official binary would be easy to start but could
  not safely own refresh, in-flight turn snapshots, TUI behavior, or additive
  protocol methods.
- An authentication proxy would introduce a second security and request
  replay boundary and would not supply complete local account-management UX.
- A differently named parallel CLI would avoid replacing the official binary
  but would break the explicit compatibility requirement for existing callers.
- A maintained downstream fork can extend upstream abstractions and preserve
  stock-client behavior, at the cost of continuous upstream merge and
  compatibility testing.

## Consequences

Downstream patches must stay narrow by subsystem. Every upstream release needs
schema comparison, the upstream suite, multi-account tests, and real remote-app
smoke testing. Modified builds report their downstream identity truthfully.

## Verification

The phase and definition-of-done gates in `HANDOVER.md` require reproducible
upstream baseline builds, stock CLI/app-server compatibility, profile storage
and concurrency tests, migration tests, remote-client smoke tests, packaging,
deployment, and rollback evidence.
