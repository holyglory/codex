# Upstream Baseline

- Repository: `https://github.com/openai/codex.git`
- Remote name: `upstream`
- Release tag: `rust-v0.154.0`
- Annotated tag object: `36eab01061df3cde5f95ec20a526777b430091ba`
- Peeled commit: `6b9826e3aa83b1a5947db50f4332cb9c65f1b340`
- Selected: 2026-09-09
- License: Apache-2.0; preserve the upstream `LICENSE` and `NOTICE`
- Downstream Rust version: `0.154.0+multi.4`
- Downstream npm version: `0.154.0-multi.4`

The tag object and peeled commit were fetched directly from the configured
upstream remote. The tag is annotated but does not contain a cryptographic
signature. `upstream-sync` moves to the peeled commit only after release gates pass.

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
   Reconcile the candidate with delivered downstream outcomes in Coordinator.
   Preserve capacity recovery through `suite::retry_after` and the packaged
   `test_capacity_response_recovers_in_same_turn` check; an upstream test that
   expects immediate failure does not replace this downstream requirement.
7. Push the exact tested candidate, then update `main` only with a freshly
   observed `--force-with-lease` value. Verify the archive tag and default branch
   before deleting fork-owned temporary branches.

No update procedure creates a release, stages npm packages, publishes npm
packages, installs binaries, or deploys a running service unless those actions
are approved separately.
