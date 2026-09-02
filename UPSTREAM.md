# Upstream Baseline

- Repository: `https://github.com/openai/codex.git`
- Remote name: `upstream`
- Release tag: `rust-v0.153.0-alpha.6`
- Commit: `e8b3253fed5aeef7e914441bc3b73b3b0a718b51`
- Annotated tag object: `22a9fdf5f01d606eff192aacb93bc3107688450f`
- Selected: 2026-09-02
- License: Apache-2.0; preserve the upstream `LICENSE` and `NOTICE`
- Downstream package version: `0.153.0-alpha.6+multi.4`

The `upstream-sync` branch is the canonical ref for the pinned upstream commit
and moves only after the release gates pass. The `multi-account` branch carries
downstream architecture and implementation.
Future upgrades must update this record only after the compatibility workflow
in `HANDOVER.md` completes for the new revision.

## Validation evidence

- Rust toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`
- Cargo: `1.95.0 (f2d3ce0bd 2026-03-21)`
- `just`: `1.51.0`
- `cargo-nextest`: `0.9.143`
- DotSlash: `0.5.7`
- Bazelisk: `1.28.1` (Linux amd64 SHA-256
  `22e7d3a188699982f661cf4687137ee52d1f24fec1ec893d91a6c4d791a75de8`)
- Bazel: `9.0.0` from the pinned `.bazelversion`
- App-server stable and experimental schemas, the core configuration schema,
  and the Bazel dependency lock regenerated and checked successfully on
  2026-09-02.
- Focused plugin and Guardian suites passed all 442 and 91 tests respectively;
  the complete Rust workspace passed all 17,352 tests with 31 intentionally
  skipped and one unrelated SQLite-startup race passing on its automatic retry.
- The release and completion-ledger scripts passed all 25 tests. Affected-crate
  Clippy/fix, formatting, snapshot review, and diff validation passed.
- Package validation and deployment were intentionally not run. This branch is
  source-validated but is not a packaged, release-ready, or deployed build.
