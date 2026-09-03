# Upstream Baseline

- Repository: `https://github.com/openai/codex.git`
- Remote name: `upstream`
- Release tag: `rust-v0.153.0`
- Annotated tag object: `6bc50f104dcc0192e696cdeae721dfc19b507391`
- Peeled commit: `41e22fee981a63b3698df7ed36bad393cda24715`
- Selected: 2026-09-03
- License: Apache-2.0; preserve the upstream `LICENSE` and `NOTICE`
- Downstream Rust version: `0.153.0+multi.1`
- Downstream npm version: `0.153.0-multi.1`

The tag object and peeled commit were fetched directly from the configured
upstream remote. The tag is annotated but does not contain a cryptographic
signature. `upstream-sync` points exactly at the peeled commit.

## Maintenance policy

`main` is the downstream product branch. It is rebuilt as a curated patch stack
on the newest explicitly approved stable `rust-vX.Y.Z` release. The stable tag
is the reproducible source boundary; upstream source wins during conflict
resolution, and downstream behavior is reapplied through current extension
points.

`upstream/main` and prerelease tags are canaries only. The scheduled downstream
watcher may report a newer stable tag and may perform an ephemeral rebase and
compile probe, but it never moves `main` or `upstream-sync`.

## Stable update procedure

1. Fetch the candidate annotated stable tag from `upstream` and record both its
   tag object and peeled commit here.
2. Create an immutable archive tag for the completed downstream head and verify
   it remotely before rewriting a branch.
3. Rebase the ordered downstream commits in an isolated worktree. Do not stash,
   reset, or overwrite an active checkout.
4. Drop obsolete compatibility patches already implemented upstream and resolve
   conflicts in favor of the stable source before reintroducing required fork
   behavior.
5. Regenerate schemas, exports, and Cargo/Bazel locks from the resolved source.
6. Run focused downstream checks, one fresh complete Rust pass, complete Bazel
   validation, local Linux packaging, and the six-platform candidate workflow.
7. Push the exact tested candidate, then update `main` only with a freshly
   observed `--force-with-lease` value. Verify the archive tag and default branch
   before deleting fork-owned temporary branches.

No update procedure creates a release, stages npm packages, publishes npm
packages, installs binaries, or deploys a running service unless those actions
are approved separately.
