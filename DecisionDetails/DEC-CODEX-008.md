# DEC-CODEX-008: Repository-local verification

## Confirmed boundary

DevCoordinator is not used for test, build, schema, runtime, or handoff work in
`/home/CodexMulti` unless the user explicitly reverses this decision. Use the
repository's local commands and preserve their output as evidence when needed.

This decision does not uninstall or modify DevCoordinator, does not govern
other repositories, and does not delete the dormant `.codex/tests.json`
configuration. If a required local workflow cannot run in the available
environment, report that limitation honestly rather than routing the work
through Coordinator.

## Evidence and rationale

The Coordinator handoff duplicated ten already-green targets, caused severe
disk pressure during concurrent Rust linking, could not be cancelled while
attempt leases were stale, and terminated abandoned. A later replacement plan
failed validation under a changed Coordinator release. The retained individual
immutable runs had already supplied green source-test evidence, so additional
Coordinator orchestration added operational risk without adding valid product
evidence.

See `UIL-TESTING-001` and superseded completion issue
`CML-B74D53A90D3C`.
