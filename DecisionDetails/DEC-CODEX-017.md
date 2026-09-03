# DEC-CODEX-017: Stable-release downstream patch stack

## Decision

Maintain one downstream product branch, `main`, as an ordered patch stack on
the newest explicitly approved stable upstream `rust-vX.Y.Z` release. Update it
by rebasing the downstream commits onto that stable tag, treating upstream code
as authoritative in conflicts and reintroducing only behavior the fork still
needs through current extension boundaries.

The selected baseline is `rust-v0.153.0`, annotated tag object
`6bc50f104dcc0192e696cdeae721dfc19b507391`, peeled commit
`41e22fee981a63b3698df7ed36bad393cda24715`, selected on 2026-09-03. The Rust
version is `0.153.0+multi.1`; npm maps the same downstream revision to
`0.153.0-multi.1`.

This decision supersedes the older pinned baselines in DEC-CODEX-003 and
DEC-CODEX-015 and the subsequently deployed alpha baseline described in the
legacy handover. It does not erase their historical release evidence.

## Options considered

- Tracking `upstream/main` directly would expose the product branch to moving,
  unreleased behavior and make artifacts difficult to reproduce.
- Treating the newest prerelease as the product baseline would repeat the alpha
  version ambiguity that prompted this rebuild.
- Preserving the accumulated merge graph would retain every historical
  workaround but keep obsolete patches, large conflict surfaces, and unclear
  ownership.
- A curated rebase makes future stable updates deliberate and reviewable, at
  the cost of resolving downstream patches again when upstream interfaces move.

## Branch and history boundaries

- `main` is the only mutable downstream product branch after acceptance.
- `upstream-sync` points exactly at the selected stable commit and moves only
  after a later stable update is approved.
- Annotated archive and downstream release tags are immutable recovery points.
- Upstream's advertised branch and tag refs remain untouched; they are not
  downstream product history.
- Temporary sync branches exist only while a candidate is being reconstructed
  and tested, and are deleted after the tested commit becomes `main`.

## Compatibility and update policy

The downstream stack preserves stock behavior when no profiles are configured,
uses additive capability-gated interfaces, limits automatic selection to
managed ChatGPT profiles, rejects conflicting external authentication, probes
capacity within bounds, applies descending priority, and fails over only where
the current turn remains safe. Usage capture remains private and content-free.

The scheduled watcher may report a newer stable tag and may rebase the patch
stack ephemerally onto `upstream/main` as a compatibility canary. Neither result
may move `main`, change `upstream-sync`, tag a release, or publish a package.

## Acceptance boundary

Promotion requires regenerated schemas and locks, focused downstream suites, a
fresh complete Rust pass, complete Bazel validation, optimized local Linux
packages, and the six-platform GitHub candidate matrix with all seven npm
tarballs verified. Candidate artifacts remain GitHub Actions artifacts only.
Installation, deployment, GitHub Release creation, npm staging, and npm
publication require separate later authority and are outside this rebuild.
