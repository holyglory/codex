# Downstream candidate validation

`downstream-candidate` remains the complete release gate. A successful older
commit is not publication evidence for a corrected commit. Candidate artifacts,
checksums and attestations must all identify the frozen source being released.

## Repair loop

1. Diagnose a failure from the retained job evidence. Let the sealed run collect
   other safe findings; do not modify its source or automatically restart it.
2. Repair in isolated state and test the changed behavior **and its consumers**.
   Run `python3 scripts/run_candidate_preflight.py` in the normal isolated local
   test environment for the desktop reconnect/input boundary. Each test family
   must select tests; a renamed or empty family fails instead of silently passing.
3. Batch related fixes, run scoped lint and formatting, and freeze the candidate.
4. Run `scripts/local_candidate.py check` in the isolated local environment
   described below. Local helpers, formatting, packaging prerequisites, focused
   tests, generated schema drift, Bazel lock and actual Bazel socket/migration
   fixture checks must pass first. Then full
   Clippy/Rust tests/Linux packaging and full Bazel validation run as separate
   sequences. An ordinary failure does not cancel the other sequence.
5. Dispatch one complete candidate run with the successful local receipt. Its identity/helper checks and focused
   preflight must succeed before full Rust, full Bazel and six native builds begin.
   Those expensive jobs then run in parallel. All are still required for npm.

The preflight currently covers saved-task recovery, bounded command-output capture,
and the dependent TUI turn
submission behavior. It is an early regression gate, not a replacement for
changed-feature tests or the complete downstream suite. Extend the focused
selection when another producer/consumer boundary changes.

For a transient failure on **unchanged source**, use GitHub's failed-job rerun,
not a new workflow dispatch. A rerun still uses that run's original commit; it
cannot validate a source correction. Investigate repeated failures rather than
automatically retrying deterministic test defects.

## Local-first Linux validation

Use a committed, clean, isolated worktree. `plan` prints the exact commands without
starting them:

```sh
python3 scripts/local_candidate.py plan --state-dir /mnt/build-storage/codex/state/local-first
```

The local driver preserves Cargo targets, Bazel output, repository downloads and
the Bazel action cache under that external directory. It never runs `cargo clean`
or the disposable GitHub-runner cleanup helpers. An optional `--cargo-target-dir`
can reuse an existing compatible compact debug/test target directory. Release
builds use a separate persistent target (`--release-target-dir` can reuse the
existing release store); tests and Clippy share the debug target.
Locks prevent simultaneous drivers from writing the same stores. Do not run
other build commands manually against those locked stores.

The full local engines run one at a time: the first simultaneous pass caused
short-deadline failures that passed when isolated after compilation ended.
This avoids a custom worker-count controller and does not relax test timeouts.
Hosted engines still run in parallel on their separate machines.
Both the focused backend-layout checks and the full graph use the same short,
neutral `/tmp/b` test root inside the isolated mount namespace.

For this Linux VPS, `scripts/run_local_candidate.sh` creates a private mount
namespace and mounts an isolated test `/tmp` on the build disk, plus an empty read-only
system configuration directory. It drops to the invoking account before running
any validation. This avoids loading installed user configuration and keeps Unix
socket paths short. It does not modify the host mounts or installed release.
The state directory must already exist and be writable by the invoking account.
Its backing test directories stay stable so a warm Bazel server sees the same
files across runs; each invocation still gets a fresh temporary `CODEX_HOME`.
Supply the existing, verified native prerequisites through these variables:

- `RUSTY_V8_ARCHIVE` and `RUSTY_V8_SRC_BINDING_PATH`: GNU/Linux test-build artifacts.
- `LOCAL_MUSL_V8_ARCHIVE` and `LOCAL_MUSL_V8_BINDING`: musl release-build artifacts.
- `LOCAL_MUSL_PKG_CONFIG`: musl libcap pkg-config directory.
- `LOCAL_PACKAGE_PYTHON`: the existing smoke-test virtualenv Python.
- `CODEX_BAZEL_BIN` (optional): an existing pinned Bazel executable when `bazel`
  is not on the build account's normal `PATH`.

Keep these prerequisite files on the build disk, **not the host `/tmp`**, because
the isolated `/tmp` hides host temporary files. With those variables configured:

```sh
sudo --preserve-env=CODEX_BAZEL_BIN,RUSTY_V8_ARCHIVE,RUSTY_V8_SRC_BINDING_PATH,LOCAL_MUSL_V8_ARCHIVE,LOCAL_MUSL_V8_BINDING,LOCAL_MUSL_PKG_CONFIG,LOCAL_PACKAGE_PYTHON \
  bash scripts/run_local_candidate.sh /mnt/build-storage/codex/state/local-first
```

Install repository tool prerequisites once before this command, including the
pinned Rust toolchain, musl target/toolchain, Clippy/rustfmt, nextest, just, Bazel,
Node/pnpm, uv and packaging tools. Missing prerequisites are failures, not skips.
Logs and `receipt.json` remain in the printed `accept-*` directory, including on
ordinary failure. Linux packaging creates stripped CLI/app-server gzip and zstd
packages, a shared symbols archive and checksums, and smoke-tests both formats.
No installation or publication is performed.

After pushing the frozen commit to its candidate branch:

```sh
python3 scripts/local_candidate.py dispatch \
  --receipt /mnt/build-storage/codex/state/local-first/accept-EXACT/receipt.json \
  --branch codex/EXACT-CANDIDATE-BRANCH
```

Dispatch rejects incomplete/failed receipts, changed logs, dirty source, a remote
SHA mismatch and an active candidate on that branch. The hosted identity job also
rejects missing or wrong-commit receipts before expensive jobs start. This is an
owner-operated workflow gate, not a cryptographic attestation that local tests
ran: all existing hosted verification, artifact provenance and publication gates
remain required. Local Linux success does not establish macOS or Windows behavior.
Those platforms and ARM builds continue using the existing GitHub-hosted runners;
no additional machines, self-hosted runners or runner credentials are configured.

## Compilation reuse and storage

- A pinned sccache installation stores content-keyed Rust compiler results in
  GitHub's cache backend for the preflight, Rust validation and all native targets.
  Setup failure leaves ordinary compilation enabled; cache-server I/O failures
  use the compiler fallback. Compiler/test failures are never converted to success.
  Disable its request-inactivity shutdown before setup and during compilation:
  a single large compilation can exceed the default ten-minute idle window even
  though it is still working. The existing job timeout still bounds execution.
  The Linux preflight's `probe_compiler_cache.py` exercises an accelerated idle
  shutdown with the real pinned cache binary and compiler, then verifies that
  disabling idle shutdown preserves a long compilation, a cache hit, and source
  invalidation. It uses a private socket and temporary local cache, not the job's
  compiler cache. Run it locally with `--sccache /absolute/path/to/sccache`.
- The full Rust job retains its existing disk reclamation between Clippy and
  tests. Compiler caching allows reusable compilation results to survive that
  cleanup without retaining a second large target tree on the runner.
- Bazel restores and saves build results, not only dependency downloads. A
  dedicated disposable-runner cache retains about 4 GiB of completed `ac`/`cas`
  entries, evicting oldest entries every ten seconds and at command boundaries.
  In-flight writes are untouched, so peak usage can temporarily exceed that
  retained-data target. The cache shrinks further when necessary to reserve
  4 GiB of free build storage. Missing/evicted entries are rebuilt normally.
- Cache saves are best effort, including after failed tests, and do not replace
  test evidence. Only the dedicated cache is uploaded; never cache `CODEX_HOME`,
  npm credentials, installed releases or user databases. Existing GitHub cache
  quota and billing settings are unchanged.

Review sccache hit/miss summaries and Bazel process/cache-hit totals alongside
job durations. The first cold build can still take hours. The September 6, 2026
baseline was about 54 minutes for Rust and 5 hours 6 minutes for Bazel; do not
claim a speedup until a subsequent hosted run measures one. Cache eviction or
quota pressure may reduce reuse without changing the required release checks.

Publication remains separate: exact candidate verification, six native packages,
seven npm tarballs, provenance and the approved human publication gates all apply.
