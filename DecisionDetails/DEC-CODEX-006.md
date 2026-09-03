# DEC-CODEX-006: Explicit account pins and external auth

## Evidence

Upstream v0.149.0 resolves `CODEX_API_KEY`, ephemeral external ChatGPT auth,
`CODEX_ACCESS_TOKEN`, and configured persistent auth in a precedence order.
It also supports workload identity, externally refreshed auth, Agent Identity,
PAT, and Bedrock modes. Without a new rule, those sources can silently defeat a
local profile selected by `--account`.

## Options considered

- Let external auth win: rejected because the user-visible process pin would
  be false.
- Let the pin silently ignore external auth: rejected because the external
  owner or automation may rely on that credential boundary.
- Reject the combination before the first model request: selected because it
  is deterministic, nonsecret, and reversible by removing either input.

## Scope

The conflict applies to externally owned process/host sources. It does not ban
API-key, PAT, Agent Identity, or Bedrock credentials intentionally stored in
the selected local account profile.

## Verification

CLI, exec, app-server, and core tests cover every external source category,
prove no request occurs on conflict, and prove the same auth modes work when
loaded from the pinned profile itself. Errors identify only the safe source
category and never credential content.
