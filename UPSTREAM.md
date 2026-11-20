# Upstream Baseline

- Repository: `https://github.com/openai/codex.git`
- Remote name: `upstream`
- Release tag: `rust-v0.170.0`
- Annotated tag object: `72f97c309fb0f78b6f35cce455adf416e6a51571`
- Peeled commit: `29eb79e9866b3262a777fdf8195c1b9e63684c91`
- Selected: 2026-11-19
- Downstream Rust version: `0.170.0+multi.2`
- Downstream npm version: `0.170.0-multi.2`

The stable tag is the reproducible source boundary. The tag object and peeled
commit must be recorded before the candidate is built.

## Stable update procedure

1. Fetch the newest explicitly approved annotated stable tag from `upstream` and
   record its tag object and peeled commit here.
2. Preserve the old downstream head with an immutable archive tag before any
   branch rewrite.
3. Use an isolated worktree and the branch name
   `codex/rebase-rust-v<stable-version>-multi`. The stable version belongs in
   the branch name; the downstream counter does not. For example, a rebase onto
   `rust-v0.170.0` uses `codex/rebase-rust-v0.170.0-multi` and starts the new
   downstream identity at `0.170.0+multi.1`.
   For later candidates on that base, check the existing `v`, `local-v`, and
   `npm-v` release tags and the published npm versions. Choose the next unused
   downstream counter above the highest reserved or published counter. An
   unpublished tag still reserves its counter. Retry a tagged candidate only
   with its original product commit.
4. An aggregate squash is allowed when it produces one candidate commit whose
   sole parent is the selected stable commit and whose tree contains the complete
   downstream behavior. Preserve the archive tag and do not reuse the previous
   release branch name for the candidate.
5. Update every release identity and governed local check from the previous base
   and counter to the new base and counter. This includes Cargo/npm versions,
   package/deployment script names and paths, Coordinator check names, and local
   package proof expectations.
6. Regenerate schemas, exports, and Cargo/Bazel locks from the resolved source.
   Run `just bazel-lock-check` and compile the native test fixtures before
   reserving the npm product tag.
7. Run focused compile/tests, the complete Rust test pass, complete Bazel
   validation, local Linux packaging, and the six-platform candidate workflow.
8. Push the exact tested candidate only after validation. Update any stable
   product branch with a freshly observed `--force-with-lease` value. Keep the
   archive tag and candidate branch until the new identity is verified.

No stable update step creates a release, publishes npm packages, installs a
binary, or deploys a running service unless that action is separately requested.
