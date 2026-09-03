# DEC-CODEX-003: Initial upstream baseline

## Evidence

On 2026-08-20, the official `openai/codex` tag listing identified
`rust-v0.149.0` as the newest non-alpha Rust CLI release. The annotated tag
object is `a4e15bf371341b067c8278d3b70b1a8c7b3d793e` and resolves to commit
`758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`.

## Options considered

- `main` and `rust-v0.149.0-alpha.*` contain newer changes but do not provide
  the approved stable release boundary.
- `rust-v0.148.0` matched examples in the original handover but was no longer
  the newest stable release when implementation started.
- `rust-v0.149.0` provides a reproducible stable source, schema, test, license,
  and packaging baseline.

## Implementation

The `upstream` remote points to `https://github.com/openai/codex.git`.
`upstream-sync` points exactly to the selected commit; downstream work occurs
on `multi-account`. `UPSTREAM.md` records the tag and commit.

## Verification

Before release, reproduce the upstream build and test/schema baselines at this
commit. Every later upstream change must update `UPSTREAM.md`, supersede this
decision, and rerun the handover's compatibility workflow.
