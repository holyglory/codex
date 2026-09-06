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
4. Dispatch one complete candidate run. Its identity/helper checks and focused
   preflight must succeed before full Rust, full Bazel and six native builds begin.
   Those expensive jobs then run in parallel. All are still required for npm.

The preflight currently covers saved-task recovery and the dependent TUI turn
submission behavior. It is an early regression gate, not a replacement for
changed-feature tests or the complete downstream suite. Extend the focused
selection when another producer/consumer boundary changes.

For a transient failure on **unchanged source**, use GitHub's failed-job rerun,
not a new workflow dispatch. A rerun still uses that run's original commit; it
cannot validate a source correction. Investigate repeated failures rather than
automatically retrying deterministic test defects.

## Compilation reuse and storage

- A pinned sccache installation stores content-keyed Rust compiler results in
  GitHub's cache backend for the preflight, Rust validation and all native targets.
  Setup failure leaves ordinary compilation enabled; cache-server I/O failures
  use the compiler fallback. Compiler/test failures are never converted to success.
- The full Rust job retains its existing disk reclamation between Clippy and
  tests. Compiler caching allows reusable compilation results to survive that
  cleanup without retaining a second large target tree on the runner.
- Bazel restores and saves build results, not only dependency downloads. A
  dedicated disposable-runner cache retains about 4 GiB of completed `ac`/`cas`
  entries, evicting oldest entries every ten seconds and at command boundaries.
  In-flight writes are untouched, so peak usage can temporarily exceed that
  retained-data target. Missing/evicted entries are rebuilt normally.
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
