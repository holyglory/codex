# DEC-CODEX-007: Shared repository permissions

## Confirmed boundary

All participating local accounts belong to one operator and are intentionally
inside the same repository trust boundary. This applies to the whole
`/home/CodexMulti` tree, including `.git`, `.state`, `target`, and caches. The
separate per-`CODEX_HOME` credential and usage-data boundary remains unchanged.
See `security-assumptions.md` and `UIL-OPERATIONS-FILESYSTEM-001`.

## Operational contract

- Owner and `vps-repo-users` receive equal `rwX`; unrelated users receive no
  repository access.
- Directories are setgid and inherit group `rwx` through default ACL entries.
- Named user ACL entries are absent; new exceptions require a changed security
  assumption or explicit user decision.
- Software-created repository state requests shared modes, while immutable
  sandboxes may independently fall back to owner-private directory or file
  modes when chmod ownership is unavailable.

## Applied evidence

The normalized tree audit reported zero owner/group mode mismatches, zero
missing setgid directories, and zero named ACL entries across 16,459
directories. A second local account wrote through the shared build target and
successfully ran completion-ledger integrity checks. Re-audit this contract
after ownership changes, archive extraction, or tools that replace directories.
