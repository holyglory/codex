# DEC-CODEX-004: Preserve upstream usage surfaces

## Evidence

Source inspection at `rust-v0.149.0` found the existing TUI `/usage` command in
`codex-rs/tui/src/slash_command.rs` and its menu/inline implementation in
`codex-rs/tui/src/chatwidget/usage.rs`. It provides service-backed daily,
weekly, and cumulative activity plus rate-limit-reset actions.

The existing public RPC `account/usage/read` is registered in
`codex-rs/app-server-protocol/src/protocol/common.rs`, implemented in
`codex-rs/app-server/src/request_processors/account_processor.rs`, and covered
by public app-server tests.

Current app-server guidance requires singular resource names of the form
`resource/method`. The original handover's plural nested suggestions were not
final contracts.

## Decision and alternatives

- Repurposing bare `/usage` exclusively for local statistics was rejected
  because it would displace existing account usage/reset behavior.
- Overloading `account/usage/read` was rejected because service totals cannot
  truthfully be attributed to local chats or repositories.
- Preserve `/usage daily|weekly|cumulative`, retain bare `/usage` as a menu,
  and add local current-chat statistics as its first enhanced item.
- Use collision-free `accountProfile*` and `localUsage*` RPC resource names,
  with optional initialization capabilities and notifications.

## Compatibility behavior

Older remote servers retain the upstream `/usage` menu and authentication
gating. Enhanced servers advertise local usage independently; local statistics
remain available without ChatGPT service auth, while account-usage/reset
actions keep their existing auth requirements. Stock clients ignore unknown
capabilities and notifications.

## Verification

Preserve existing usage snapshots and account-usage RPC tests. Add capability
handshake, old-server fallback, local-menu ordering, inline command, RPC schema,
and unknown-notification compatibility coverage.
