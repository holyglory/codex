# DEC-CODEX-010: Slawa-only initial deployment

## Confirmed scope

Install the customized `codex` release only for the Unix account `slawa` on the
owner-operated VPS. Do not change the launchers, current-release links, runtime
data, or processes of `holyglory`, `holygloryTT`, or `axel`.

The shared repository access decision in DEC-CODEX-007 remains unchanged: all
four accounts retain equal source-tree rights. This decision narrows binary
deployment only.

## Operational contract

- Build and checksum one immutable `0.149.0+multi.1`
  `x86_64-unknown-linux-musl` package through repository-local workflows.
- Require a delivery plan that names only `slawa`, records exact rollback
  component hashes, and observes no active `slawa` Codex process before atomic
  activation.
- Preserve credentials and usage data; installation changes only versioned
  package files and the managed launcher/current symlinks.
- Verify the installed version, safe account and usage diagnostics, MCP,
  app-server capabilities, thread/history access, and rollback readiness under
  `slawa`.
- Record non-target launcher/current state before and after deployment to prove
  the other three installations were not changed.

See completion issue `CML-0182C9852081`.
