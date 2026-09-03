# DEC-CODEX-016: Public downstream npm distribution

## Decision

Publish the maintained downstream CLI from the public GitHub fork
`holyglory/codex` as the public scoped npm package `@holyglory/codex`. Preserve
the executable name `codex` and the existing platform-split package design so
installations continue to download only the native payload for their operating
system and architecture.

The candidate workflow builds and verifies all seven tarballs without npm
authority. Publication is a separate `downstream-npm-publish.yml` workflow that
accepts only an annotated immutable tag matching the tested candidate commit,
uses the protected `npm` GitHub environment for manual approval, and obtains
short-lived npm authority through OIDC. Do not retain an npm write token as
steady-state release infrastructure.

## Options considered

- Continuing with server-local binary deployment would not provide the
  requested standard `npm install -g` distribution path.
- Publishing under `@openai/codex` is unauthorized and would conflate the fork
  with OpenAI's official package.
- A single all-platform tarball would make every installation unnecessarily
  large and discard the repository's existing platform selection behavior.
- Direct unattended npm publication would remove the requested human release
  gate. A protected publication workflow keeps final release authority with the
  operator while leaving ordinary candidate builds unable to publish.
- A permanent npm automation token would work technically but creates a
  long-lived credential that must be stored, rotated, and protected. GitHub
  Actions OIDC provides short-lived workflow-bound authority instead.

## Distribution and compatibility boundaries

- The npm package is an unofficial downstream distribution and must say so in
  its description and README.
- OpenAI service authentication, subscription, workspace, usage, and
  rate-limit terms are unchanged by the fork's Apache-2.0 source license.
- Package contents include only the launcher, canonical native package
  artifacts, README, `LICENSE`, `NOTICE`, and npm metadata. Local credentials,
  account registries, usage databases, logs, caches, and deployment state are
  prohibited.
- All six existing npm targets remain in the publication set: Linux x64/ARM64,
  macOS x64/ARM64, and Windows x64/ARM64. Every platform version must be staged
  and approved before the root `latest` wrapper.
- GitHub's standard public hosted runners are the release build boundary.
  Trusted publishing supplies npm provenance through the approved OIDC-bound
  workflow.
- Rust binaries retain the truthful `+multi.N` build metadata. Because npm 11
  strips SemVer build metadata during publication, npm package versions encode
  the same downstream revision as `-multi.N`; the release verifier rejects a
  `+` npm version before submission.

## Verification

Before any submission, verify the full seven-tarball set, package identity,
repository metadata, public registry, platform constraints, optional
dependency aliases, native executable paths, and redistribution files. Install
and run the platform package through a clean npm prefix on each supported
target. Publication completes only after npm shows every platform version and
the root wrapper, a clean external installation resolves `codex`, and the
reported version matches the approved release.
