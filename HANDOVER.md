# Stable Fork Handover

## Product boundary

This repository is the unofficial `holyglory/codex` downstream fork. Its
product branch is based on the stable upstream `rust-v0.153.0` release and
reports Rust version `0.153.0+multi.1`. The matching npm version is
`0.153.0-multi.1`.

This baseline produces a tested candidate only. It does not install Codex,
restart an app server, change any user's active release, create a GitHub
Release, stage npm versions, or publish to npm.

See `UPSTREAM.md` for the verified tag object, peeled commit, stable update
policy, and branch transition procedure. See `DecisionHistory.md` for durable
product choices and `UserIssueLedgers/` for corrections that future work must
not repeat.

## Repository recovery points

- `archive/pre-stable-0.153.0-restructure` is the immutable annotated archive
  of the completed legacy line at
  `9849aad25e971934d10e4312c0a86f9ef0ae2690`.
- Existing downstream release tags remain immutable.
- `upstream-sync` identifies the approved stable upstream commit.
- `main` is the only downstream product branch after remote acceptance.
- Upstream-identical advertised refs are mirrors, not fork-owned product
  history, and are intentionally left untouched.

The archive tag must be verified remotely before any branch rewrite. The final
`main` update must use a freshly observed exact `--force-with-lease` value; a
plain force push is not an accepted recovery shortcut.

## Downstream patch order

The downstream history is kept as an ordered, reviewable stack:

1. Fork version identity, private-storage primitives, and workspace membership.
2. Account registry, profile persistence, and legacy credential migration.
3. Explicit and automatic selection, bounded rate-limit probes, priority, and
   safe turn failover.
4. Content-free usage facts, private SQLite persistence, lifecycle capture,
   recovery, reporting, and repository attribution.
5. CLI, app-server, agent-tool, extension, and TUI integrations.
6. npm packaging, downstream-only CI, candidate artifacts, and operating docs.

During future stable rebases, keep upstream source when an interface has moved,
then reapply the required behavior through the current extension point. Drop a
downstream compatibility patch when the selected stable release already
implements it.

## Required compatibility contracts

- Voice startup resolves the same managed-profile selection before any media
  request, including remote clients with no prior text turn. Call creation and
  control connections use the same selected credentials; the call retains its
  profile lease until stopped, including control reconnections. Later default
  changes apply to later calls. Selection failure must not fall back to an empty
  legacy login or demand a separate API key for a managed ChatGPT profile
  (UIL-BUSINESS-LOGIC-ACCOUNT-SELECTION-005).
- With no configured profiles, stock upstream authentication and behavior are
  unchanged.
- Downstream app-server methods remain additive, experimental, and capability
  gated. Stock clients do not receive downstream-only notifications.
- Automatic selection and automatic failover consider only locally managed
  ChatGPT OAuth profiles.
- An explicit profile pin fails visibly when an externally managed auth source
  would override it.
- Unknown or stale capacity is refreshed through bounded read-only probes
  before selection. Unknown capacity does not silently become eligible.
- Higher numeric priority drains before lower priority; the current profile is
  retained only within its eligible priority tier.
- Mid-turn failover is allowed only before any response event. Tools and
  partial output are never replayed under another profile.
- Usage accounting stores content-free facts in the private `CODEX_HOME`
  boundary and distinguishes missing measurements from zero.

## Rollout ordinal and projection recovery

Upstream commit `095ac4f131e759b204fa6368dc42d2feff6eb21a` is backported with
its original authorship and `GitOrigin-RevId`. It lets the derived SQLite thread
history projection skip malformed, unknown, missing, duplicate, regressed,
gapped, or timestamp-invalid records while continuing to materialize later
history.

Writer-side resume discovery reads a minimal top-level ordinal envelope instead
of deserializing the complete flattened rollout payload. Consequently, a valid
unknown future record or a record containing unsupported floating-point payload
fields still consumes its ordinal. Invalid unterminated crash tails retain the
existing repair behavior. A complete valid final record without a usable
unsigned ordinal fails closed, and `u64::MAX` remains a hard append failure.

Existing rollout JSONL is canonical and is never rewritten by this repair. The
SQLite view is derived and may recover after a future deployment, which is not
part of this baseline.

## Candidate and npm workflows

- `downstream-ci.yml` runs formatting, focused Clippy, generated-file and lock
  drift, downstream suites, and Linux compilation for pull requests and
  `main`.
- `downstream-candidate.yml` runs complete Rust and Bazel validation, builds six
  native release targets on standard public runners, archives symbols, creates
  gzip and zstd canonical CLI and app-server packages, writes checksums and
  provenance attestations, and verifies seven npm tarballs.
- `downstream-upstream-watch.yml` reports a newer stable release and performs an
  ephemeral `upstream/main` rebase/compile canary without moving a branch.
- `downstream-npm-publish.yml` is separate from candidate construction. Its
  staging job defaults off and requires an annotated matching tag, the exact
  successful candidate run, OIDC, and approval through the protected `npm`
  environment.

The root npm artifact is `@holyglory/codex@0.153.0-multi.1`. Its six optional
dependency aliases select platform versions of the same package:

- `0.153.0-multi.1-linux-x64`
- `0.153.0-multi.1-linux-arm64`
- `0.153.0-multi.1-darwin-x64`
- `0.153.0-multi.1-darwin-arm64`
- `0.153.0-multi.1-win32-x64`
- `0.153.0-multi.1-win32-arm64`

Every tarball contains the applicable launcher or native payload, `LICENSE`,
`NOTICE`, downstream repository metadata, and exact OS/CPU or alias metadata.
Candidate artifacts are retained only as GitHub Actions artifacts.

## Local candidate verification

Use repository-owned commands from the isolated candidate worktree. Regenerate
configuration and stable/experimental app-server schemas, refresh Cargo and
Bazel locks, run the focused suites named in the accepted plan, then run one
complete `just test` and one complete Bazel validation over the frozen source.

The local release build targets `x86_64-unknown-linux-musl` and includes
`codex`, `codex-app-server`, `codex-code-mode-host`, and bundled `bwrap`. Keep
the pre-strip symbols, strip the packaged CLI and app server, create gzip and
zstd archives plus checksums, and smoke-test both compression forms with a new
temporary `CODEX_HOME`. Do not point smoke tests at an installed user's home.

## Deliberately omitted legacy infrastructure

The environment-bound legacy deployment scripts and repository-local SQLite
completion ledger remain recoverable through the archive tag but are not part
of the stable patch stack. They hard-coded an alpha version and specific local
accounts, and the project now uses DevCoordinator2's append-only database as
the authoritative completion ledger. This omission does not authorize a new
deployment implementation or weaken any deployment gate.

Unrelated ledger work, including cold all-history report performance, remains
open and is not converted into a release-readiness claim by this rebuild.
