# DEC-CODEX-009: ChatGPT-only automatic failover

## Confirmed policy

Automatic account selection accepts only locally managed `AuthMode::Chatgpt`
profiles. Personal access tokens, API keys, Bedrock credentials, Agent
Identity, request-header auth, and externally supplied ChatGPT tokens remain
available through explicit manual selection or process pinning but are never
entered automatically.

A manually activated excluded profile is used when automatic selection is
off. A process-local `--account` pin remains authoritative regardless of the
global automatic-selection setting. The CLI and TUI disclose this distinction
instead of promising next-turn use while automatic selection is enabled.

The broader `has_chatgpt_account` classification remains valid for backend and
rate-limit behavior and must not be used as the automatic-selection billing
boundary. Account-selection code owns an exhaustive, narrower predicate.

## Verification boundary

Selection tests cover every authentication mode, proving that an eligible
ChatGPT OAuth profile may be retained or selected and that every other mode is
excluded from automatic candidates without changing manual selection.

This decision resolves `CML-510820670966` once that exhaustive behavior is
green.
