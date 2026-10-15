# Upstream Baseline

- Repository: `https://github.com/openai/codex.git`
- Remote name: `upstream`
- Release tag: `rust-v0.162.1`
- Annotated tag object: `7a078f6bb4ce0dc2a81408b334ce52979d92c603`
- Peeled commit: `092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`
- Selected: 2026-09-29
- License: Apache-2.0; preserve the upstream `LICENSE` and `NOTICE`
- Downstream Rust version: `0.162.1+multi.1`
- Downstream npm version: `0.162.1-multi.1`

The tag object and peeled commit were fetched directly from the configured
upstream remote. The tag is annotated but does not contain a cryptographic
signature. `upstream-sync` moves to the peeled commit only after release gates pass.

## Maintenance policy

`main` is the downstream product branch. It is rebuilt as a curated patch stack
on the newest explicitly approved stable `rust-vX.Y.Z` release. The stable tag
is the reproducible source boundary; upstream source wins during conflict
resolution, and downstream behavior is reapplied through current extension
points.

`upstream/main` and prerelease tags are not product baselines. Stable updates
are initiated manually from an explicitly approved annotated stable tag and
never move `main` or `upstream-sync` before the release gates pass.

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
7. Push the exact tested candidate, open its stable-refresh pull request, and
   merge through the reviewed pull request. Do not rewrite `main`. Verify the
   rollback tag and merged commit before retiring temporary branches.

No update procedure creates a release, stages npm packages, publishes npm
packages, installs binaries, or deploys a running service unless those actions
are approved separately.

## Rebase lessons learned

Keep this section current when a stable update exposes a repeatable failure
mode. These are engineering notes for the next rebase; the Coordinator remains
the authority for release outcomes and verification receipts.

- **Start with a patch inventory and range-diff.** Rebase the ordered fork stack
  in an isolated worktree, then compare each replayed commit with its original
  using `git range-diff`. A downstream commit may have a new hash while still
  being the same patch. In particular, verify known commits such as `d6e5e7687f`
  by patch identity before applying them again.
- **Verify the toolchain before diagnosing compile failures.** Read
  `codex-rs/rust-toolchain.toml`, resolve the actual `rustc` and `cargo` selected
  by the shell, and compare them with the toolchain used by DevCoordinator's
  governed checks. On this rebase the shell selected Rust 1.94.1 while the
  repository requires 1.95.0; that mismatch made a stable `cfg_select!` test
  appear to be a source regression. Put the pinned toolchain first in `PATH`
  (or use the governed environment) before changing code or interpreting test
  failures.
- Treat an installed toolchain as availability, not selection. Before each
  diagnostic or broad validation pass, retain the selected executable identity
  and versions with `command -v rustc`, `command -v cargo`, `rustc --version`
  and `cargo --version`, then compare them with `rust-toolchain.toml` and the
  governed check's declared `PATH`. A candidate is toolchain-verified only
  when the shell and governed runner both select the pinned channel; changing
  `PATH`, `HOME`, `TMPDIR`, or Cargo target/cache settings later creates a new
  environment and requires a fresh validation receipt.
- **Make the toolchain check a hard preflight, not a remembered instruction.**
  The release runner must print the resolved `rustc`/`cargo` paths and versions,
  compare both binaries with `rust-toolchain.toml`, and stop before compiling
  when they differ. This rebase had Rust 1.95.0 installed but the shell still
  selected `/usr/bin/rustc` and `/usr/bin/cargo` 1.94.1; because the canonical
  runner and its preflight were omitted from the replay, the same mismatch was
  rediscovered during testing. Keep the preflight and its mismatched-toolchain
  fixture in the release workflow, and use the canonical isolated runner rather
  than a hand-written substitute test graph.
- **Do not restore downstream behavior by replaying broad WIP checkpoints.**
  The 0.162.1 forward-port checkpoints carried downstream account, usage, event,
  and plugin code but also reverted or omitted newer stable interfaces. Replaying
  one wholesale produced duplicate modules, stale SQLite constructors,
  `PathUri`/`AbsolutePath` mismatches, and missing authenticated plugin methods.
  Their companion adaptation chains also regressed upstream behavior. Preserve
  those checkpoints for diagnosis. Restore bounded behavior from the published
  rollback source through current upstream interfaces, then review semantics and
  verify the affected journeys. Compilation alone does not prove inclusion.
- **Pin native inputs in the release runner too.** This line's voice-host
  crates require GStreamer 1.28; the system package is older. Use the retained
  `model-capacity-voice-tests` native/pkg-config and ALSA inputs, with their
  matching `PKG_CONFIG`, `PKG_CONFIG_LIBDIR`, and `LD_LIBRARY_PATH`, in every
  full release graph. A plain shell compile can fail before tests even start
  while the established native fixture is already sufficient.
- **Pin the V8 archive and generated binding.** The V8 crate may request a
  release asset URL that no longer exists for the target even though the
  verified archive is already retained locally. Set both `RUSTY_V8_ARCHIVE`
  and `RUSTY_V8_SRC_BINDING_PATH` from the retained `rusty-v8-150.4.0` fixture
  before full Rust or package validation; do not treat the download 404 as a
  product failure.
- **Keep the V8 feature aligned with the linked artifact.** The hosted Rust
  lane links the sandbox-enabled V8 archive, so its complete `just test` must
  pass `--features codex-v8-poc/sandbox`. The local candidate runner previously
  excluded `codex-v8-poc`, which let local validation pass while hosted CI
  failed `sandbox_feature_matches_linked_v8`. Keep both runners on the same
  feature selection and include this canary in the release receipt.
- **Check temporary-storage inodes, not only free bytes.** A direct TUI
  suite had enough `/tmp` capacity by bytes but could not create SQLite and
  PTY fixtures because stale test directories consumed nearly every tmpfs inode.
  Before a broad run, record `df -h` and `df -i` for the actual temporary mount,
  use the approved private bulk scratch mount for large transient trees, and
  use Coordinator's exact-target cleanup after verifying ownership and active
  references. Age alone is never disposal proof. Route runtime tests through
  Coordinator with the isolated runner. Storage-full errors explain affected
  filesystem operations; investigate stack overflows and snapshot regressions
  separately rather than attributing every failure to storage pressure.
- **Review the resolved code before testing.** Check account routing, auth
  ownership, API error mapping, generated protocol exports, test fixtures, and
  package version metadata before spending time on the full suite. Resolve
  upstream interface changes at the current extension point instead of keeping
  an old compatibility shim.
- **Trace a missing test family to its production behavior.** A zero-test
  reconnect filter exposed both a deleted suite and missing `turn/start` and
  `turn/steer` recovery calls. Restore the original journey; do not substitute
  an unrelated passing filter. A worktree migration and path-normalization test
  likewise cannot prove worktree capture, token ownership, filtering or reports.
- **Preserve runtime initialization as well as methods.** Cooperative upgrade
  code compiled without installing `MaintenanceGate` in each session. Every
  prepare request then incorrectly reported `alreadyPausing`. Exercise the
  public prepare/cancel/restore journey after replaying lifecycle adapters.
- **Reuse the isolated runner and compatible build stores.** Verify the pinned
  toolchain and retained GNU/musl V8, libcap and smoke-test Python paths from
  `CI.md` before submission. Use the existing governed isolated test environment
  and warm Cargo target with the same package/feature selection. A new worktree
  does not require a fresh target directory; do not assert isolation with an
  environment flag while host configuration remains visible.
  Carry both V8 artifact variables into the governed environment explicitly;
  a warm target can conceal a missing prerequisite until another target or
  package selection rebuilds V8. Preserve the package/feature union between
  focused runs to avoid rebuilding shared dependencies.
- **Freeze editor setup with the candidate.** Initialize navigation metadata
  before sealing a worktree, or navigate the mutable integration checkout.
  Creating `.serena/` during a governed run changes its source fingerprint even
  when product files are untouched and invalidates release evidence. Reuse a
  completed check's worktree for the next committed candidate after its run is
  terminal; keep the earlier source identity and retained logs.
- **Retain profile authentication for realtime sessions.** Restore the public
  realtime profile suite alongside normal-turn authentication. Compile success
  cannot detect a voice call using the process auth manager instead of the
  selected profile. Check default changes, process pins, automatic selection,
  exhausted capacity, reconnect credentials, and lease release after Stop.
- **Keep Bazel helper binaries on the runtime path.** App-server tests may
  stage `codex-code-mode-host` in a separate Bazel package. In debug/test
  builds, resolve the inherited `CARGO_BIN_EXE_codex-code-mode-host` before
  deriving a sibling path from `current_exe`; otherwise code-mode integration
  tests fail only under Bazel even when Cargo and the runfiles manifest contain
  the helper.
- **Update test doubles when upstream traits gain context parameters.** The
  stable refresh changed workspace-routing resolution to receive the selected
  auth manager. Search every test-only resolver implementation after a trait
  merge; otherwise the release suite can fail at compile time long after the
  product implementation itself builds.
- **Keep profile auth and MCP resource authority on the captured turn.** MCP
  plugin discovery, recommendations, runtime refreshes, failover step capture,
  and widget `originCallId` reads must use the turn's account lease and binding.
  A compile-only pass will miss this: run the profile/failover and widget
  integration journeys, and bind the exact step client before recording the
  successful tool-call origin.
- **Regenerate stable and experimental protocol artifacts separately.** The
  normal schema command refreshes checked-in TypeScript/JSON and the stable
  compressed export; append `--experimental` to refresh the experimental
  compressed export. Run the protocol fixture suite after both commands so a
  stale binary export cannot survive a source API change.
- **Account routing must be resolved before auth is paired.** When a request can
  select a profile or workspace, clone the current request config, apply the
  current-account routing policy, and only then construct connector auth. Pairing
  auth with the pre-routing config silently loses account policy on connector
  requests.
- **Capacity error mapping is deliberately narrow.** Only HTTP 503 bodies with
  `server_is_overloaded` or `slow_down` are capacity overloads. Keep the unknown
  503 case covered separately so a generic service-unavailable response does not
  accidentally acquire capacity semantics during conflict resolution.
- **Refresh generated protocol output immediately.** After resolving an
  app-server or protocol conflict, regenerate stable and experimental schemas,
  TypeScript exports, and any combined schema fixtures before running broad
  tests. Schema drift otherwise obscures the actual source conflict.
- **Treat stale fixtures as a distinct repair class.** Rebase changes can alter
  instruction text, tool hashes, provider metadata, shell argument ordering,
  and UI snapshots without changing the intended behavior. Inspect each diff;
  update only fixtures whose expected output follows from the resolved source.
  Do not accept all snapshots blindly.
- **Guardian WebSocket fixtures must model the pool.** Test prewarm uses
  `INITIAL_WEBSOCKET_CONNECTIONS = 2`, and the helper server handles one socket
  at a time. Use separate scripted servers behind the existing proxy helper,
  keep every returned server alive until the test no longer needs it, and allow
  either pooled socket to serve classification. An unused warm socket may stay
  open; wait for the socket that actually delivered the response.
  A single server with two scripted connections can leave the test waiting on a
  socket that was never accepted.
- **The Guardian `thread_context` opt-out is no longer a legacy mode.** Stable
  upstream ignores the setting and keeps thread-owned context enabled. A test
  named `legacy_*` that expects the opt-out to omit context is stale after this
  change; rename it around the thread-owned behavior or remove it in favor of
  the existing incompatible-compaction coverage. Do not “fix” the production
  code to restore the removed mode.
- **Preserve submission-loop acceptance tests when settings APIs move.** The
  standalone settings test must exercise the submission loop and verify that
  acceptance is delivered before the queued event is released. Replacing it
  with a direct `thread_settings::update` call changes the synchronization
  contract and produces a false full-suite failure.
- **Retain asynchronous test evidence.** When a focused test times out, first
  distinguish startup/catalog, handshake/prewarm, request delivery, and score
  publication. Add bounded diagnostics, reproduce the exact await point, then
  remove diagnostics before committing the fixture repair.
- **Preserve fixture configuration while isolating host settings.** The 0.159.1
  Guardian helper wrote its scoring configuration and then cleared the entire
  configuration stack, disabling prewarm and causing 25 identical timeouts.
  Isolate host configuration at the runner boundary; do not erase the fixture's
  own layers. Name the failed await boundary in diagnostic errors.
- **Review automatically merged test setup as well as conflicts.** An upstream
  hook fix and the existing downstream fix both removed the same gate file. The
  second removal aborted the test; the mock-server cleanup then reported a
  misleading missing request. Trace the first failure before changing timing
  limits, request counts, or production behavior.
- **Snapshot provenance comes from source and fixture setup.** Built-in usage,
  account, and alarm tools are intentional fork output. Their names appearing
  in a snapshot do not establish host contamination. An empty temporary workspace
  does not supply AGENTS.md. Review those setup facts and the individual diff
  before accepting or rejecting a generated snapshot.
- **Test the current Guardian contract.** Disabling parent-compaction reuse in
  thread-owned mode requires synchronous review. Verify rejection of cached
  approval using the existing failure fixture; do not treat a 30-second timeout
  or missing classifier request as success for the removed legacy mode.
- **Make descriptor-denial fixtures portable.** On Linux, `/dev/fd` is a symlink
  into procfs and Bubblewrap cannot mount a deny mask on that symlink. Hide
  `/proc` in the disposable fixture to exercise environment replay, retaining
  stdin, output, exit-status, and protected-file checks. Include stderr and exit
  status in failure assertions so sandbox setup errors are visible.
- **Keep environment failures separate from source regressions.** The full
  suite may require the managed GStreamer plugin path and the repository's
  native test environment. `just fmt` can also fail when `uv` cannot write its
  global cache; run the Rust formatter directly to verify Rust changes and
  report the Python cache failure instead of changing project code.
- **Validate in increasing scope.** Run the rebase code review first, then
  focused governed checks for each conflict repair, then one frozen complete
  Rust/Bazel candidate validation. Do not use a full-suite failure caused by
  stale snapshots, missing native plugins, or saturated Code Mode fixtures as
  evidence of a production regression without reproducing the affected path.
- **Keep one reproducible runner and reuse build outputs.** Resolve all GNU and
  musl V8 inputs, the musl `libcap.pc` directory, smoke-test Python, and helper
  binaries before starting acceptance. Keep Cargo profile and package selection
  consistent to reuse dependencies. Run through Coordinator so chat interruption
  does not terminate the job. Mount private build-volume scratch at `/tmp`, create
  the isolated `CODEX_HOME`, and hide host `/etc/codex`. Do not include snapshot
  refresh commands in an acceptance graph. A passed selected check is diagnostic
  evidence; publication still requires the complete exact-commit receipt.
- **Classify a complete-suite failure before repairing it.** Group failures by
  their shared fixture and first failed operation. Preserve the bounded failure
  index and reproduce a representative case. Do not label consistent failures
  as load, races, or host contamination without evidence that distinguishes
  those causes from deterministic setup errors.
- **Keep fixture boundaries explicit.** Generic upstream core fixtures disable
  downstream local control tools; account, usage, alarm, and event suites opt in.
  Preserve those opt-ins through rebases. Re-enabling the tools globally changes
  unrelated tool lists, prompt layouts, and context-budget tests. Do not accept
  broad snapshot changes to hide that fixture mismatch.
- **Check runner contracts before freezing.** Cargo exclusions require
  `--workspace`; package-scoped diagnostics should not add it. Run the local
  namespace probe through Coordinator before the complete candidate. The
  source-owned bubblewrap runner preserves isolation without requiring `sudo`
  under the executor's `NoNewPrivileges` setting. Use existing compatible
  caches and targets; a new evidence directory does not require a cold build.
  A failing sandbox test remains unverified when skipped: fix its boundary or
  retain the explicit missing acceptance instead of reporting a successful test.
- **Freeze all candidate inputs.** Even a workflow-only commit changes the
  candidate identity. Make remaining repairs in a separate checkout, integrate
  them after the accepted run finishes, and bind the next run to that commit.
  The reusable correction is recorded as
  `rebase-fixture-and-runner-preservation-20261013` in Coordinator.
- **Stage companion executables in direct Bazel fixtures.** A raw app-server
  test launcher can find `codex-app-server` while its Code Mode host remains in
  a separate Bazel runfiles directory. Stage both binaries beside the launched
  server, or use the shared test-server builder that already recreates the
  installed sibling layout. Otherwise the host spawn fails with `ENOENT` and
  the test later reports only a misleading readiness timeout.
- **Package and publish only from the tested commit.** Confirm the release
  version in Cargo and npm metadata, stage all seven platform tarballs from the
  candidate artifact, verify checksums and smoke tests with a temporary
  `CODEX_HOME`, and check npm authentication before attempting publication.
- **Large single-item WebSocket fixtures must follow the batching contract.**
  The fork stages context in 4 MiB chunks and falls back to HTTP when an
  indivisible input item is larger than that target. Adapt upstream tests that
  put a 15 MiB instruction item on a WebSocket: exercise the HTTP message-budget
  path while retaining yield and wait assertions, and keep the separate
  WebSocket-to-HTTP fallback integration coverage. Do
  not weaken the production fallback or expect a WebSocket request that the
  transport is designed not to send.
- **Startup and generation WebSocket fallback must agree.** A 426 or 5xx
  response during preconnect must activate the sticky HTTP fallback before the
  first user turn. If preconnect handles only 426, a 5xx gateway failure causes
  one extra failed WebSocket attempt; keep both cases in the focused fallback
  integration group.
- **Deployment and npm are independent gates.** A healthy checkout or package
  build does not prove local deployment when DevCoordinator has no declaration
  for the repository. Likewise, `npm whoami` must succeed before staging a
  publish; an npm `E401` is an authentication blocker, not a package failure.
- **Package smoke tests are the capacity contract.** The local `0.162.1+multi.1`
  Linux package initially caught a real regression that unit tests missed:
  `response.failed` with `slow_down` was mapped to `rateLimitExceeded` instead
  of the shared capacity retry boundary. Keep both `server_is_overloaded` and
  `slow_down` in the packaged gzip and zstd smoke matrix; a passing source build
  alone is not enough.
- **Reapply downstream npm metadata after upstream packaging changes.** The
  stable refresh restored the upstream `@openai/codex` staging helper and
  silently dropped the fork's `@holyglory/codex` package aliases, redistribution
  files, and SemVer build-metadata guard. Run the package staging fixtures before
  candidate validation; they must assert the six platform aliases, `LICENSE` and
  `NOTICE`, the downstream package identity, and rejection of `+` versions. A
  passing Rust build cannot prove that npm publication still targets the fork.
- **Later visual-layout commits can stale earlier semantic assertions.** The
  `e8fdbf1f7c` borderless session-header change followed the startup-tip test
  and made its old “no model/no border” checks wrong even after the snapshots
  were refreshed. When a rebase includes a later UI layout commit, inspect the
  feature history and update behavioral assertions to the current contract;
  snapshot refresh alone is not sufficient.
- **Upstream update code is a separate fork boundary.** The 0.162.1 replay
  restored the upstream updater, registry endpoint, version parser, cache
  filename, and release-note links even though the npm launcher still named
  `@holyglory/codex`. A passing Rust suite did not catch this; the packaged
  updater smoke caught it only after linking the release binary. Keep the
  fork channel in `update_action`, `npm_registry`, `update_versions`,
  `updates_cache`, doctor diagnostics, and visible update notices. The release
  inclusion manifest now owns those files and runs focused command, parser,
  cache, and snapshot checks before the expensive candidate graph.
- **Snapshot commits require fresh Bazel test results.** The release graph
  reused a cached TUI shard after the fork update prompt snapshot changed, so
  Cargo showed the new rendered text while Bazel still compared the old
  snapshot. The local candidate Bazel lane now passes
  `--nocache_test_results`; keep that option whenever release evidence covers
  committed snapshot changes.
- **Declare the top-level TUI snapshot tree explicitly.** The broad
  `src/**/snapshots/**` test-data glob still allowed a Bazel runfile to compare
  an old upstream update prompt against the current fork snapshot. Keep
  `src/snapshots/**` as a direct test-data input and retain uncached Bazel test
  results for stable rebases.
- **Run package checks as part of local acceptance.** Declaring a check in the
  inclusion manifest was not enough: the fork updater smoke check could be
  skipped until a later package workflow. The local candidate now has an
  explicit `fork-update` lane immediately after Linux packaging and validates
  manifest package-check names before any expensive release result is accepted.
  Candidate runs also set `INSTA_WORKSPACE_ROOT` and `INSTA_UPDATE=no`, so
  snapshot evidence resolves against the frozen checkout instead of an
  ambient workspace or stale generated result.
- **Gate expensive release matrices behind shared validation.** The hosted
  candidate previously started all six native builds after only focused
  preflight, while the full Rust and Bazel jobs were still running. Schema
  regeneration also happened after the complete Rust suite, and `cargo clean`
  discarded Clippy output before that suite. Keep schema and lock checks in
  preflight, preserve the target directory unless storage pressure requires a
  cleanup, and require both shared validation jobs before starting platform
  builds. Cancel superseded branch candidates; preserve immutable main and tag
  evidence.
- **A complete-suite failure can still have a clean release-focused slice.** On
  `t20261001T041035Z-cb1d20`, 21,467 of 21,509 tests passed; the 41 failures and
  one timeout clustered in Code Mode pressure, Guardian async scorer load,
  wiremock request-count races, stale pending-input and skills snapshots, and
  missing voice jitter-buffer support. Reproduce each cluster in a focused
  governed check before changing production code; do not turn a saturated
  full-suite run into a broad rebase patch.
- **Process-group cancellation has an expected exit race.** A cancelled shell
  can run its SIGTERM cleanup trap and disappear before the follow-up direct
  kill reaches the leader. Treat `ENOENT` from that redundant kill as a
  successful cleanup; otherwise a large feature-enabled suite can report a
  false release failure even though the descendant group was handled.
- **Package smoke needs loopback permission.** The Linux candidate build can
  produce valid archives while its in-sandbox smoke run reports
  `PermissionError: [Errno 1] Operation not permitted` when the fixture binds
  `127.0.0.1`. Rerun the unchanged smoke artifacts with the host-approved
  loopback capability, then retain the gzip and zstd XML receipts.
- **Rollout helpers must follow the candidate version.** A staging helper that
  retains a previous release's literal package version rejects the current
  archive after all build checks pass. Derive the expected version from the
  checked-out workspace and compare it with the archive metadata.
- **Verify the daemon binary separately from the CLI binary after rollout.**
  A stale manually launched app-server can remain on the control socket while
  the launcher and proxy select a newer CLI. That mixed pair caused Desktop's
  model-list and message paths to fail even though each binary started alone.
  After switching releases, require `codex app-server daemon version` to report
  matching `cliVersion`, `managedCodexVersion`, and `appServerVersion`, and
  stop unmanaged app-server processes before testing the Desktop protocol.

- **Verify usage reporting through its public boundary.** A warming response
  proves bounded behavior, not usable reporting. Exercise finite-window reviews
  during cache rebuilding and after readiness, with deterministic histories of
  at least 1.3 million operations and 4.3 million observations. Independently
  calculate expected totals; preserve half-open windows, late facts, corrections,
  covered tools, descendants and pagination. Include cancellation checks that
  demonstrate SQLite stopped and released its snapshot while capture continues.
- **Test effective-UID home resolution at the installed boundary.** A root
  launch with an inherited non-root `HOME` must use UID 0's passwd home when
  `CODEX_HOME` is unset; an explicit `CODEX_HOME` remains authoritative. The
  usage database and sidecars must remain owner-only. A helper-only test or a
  non-root fixture is insufficient: run the packaged binary as UID 0, verify
  open/migrate/reopen and private modes, and ensure the ordinary-user path
  still uses its own home. Keep root data separate; never fix this by
  broadening permissions or changing ownership. The local candidate acceptance
  graph runs `scripts/check_root_usage.py` through `scripts/local_candidate.py`
  against the packaged Linux binary; a source-only resolver test does not
  satisfy this gate. The hosted downstream candidate graph repeats the check
  against the extracted x86_64 Linux archive's manifest-declared entrypoint
  before npm and final candidate completion.
- **Diagnose SQLite with its WAL attached.** Use a consistent online backup for
  private recovery; independently copied database/sidecar files can miss committed
  facts. Inspect per-source cursors and the failing page. Multiple effective
  classification roots can duplicate a projected owner and stop a rebuild even
  with ample disk space. Upgrade source-owned derived views and resume compatible
  cursors; never delete raw history or manually mark a live cache ready.
- **Place release state on the approved bulk volume before building.** Check
  capacity on every actual output mount, including Cargo, Bazel output, action
  cache and temporary files. Free space on the bulk disk does not protect `/tmp`
  or the root filesystem. Retain warm build stores and exact failure evidence;
  preserve working source and installed rollback packages during cleanup.
- **Publish from the immutable npm tag.** After exact-commit validation and live
  recovery, create the annotated `npm-v<version>` tag and dispatch
  `downstream-npm-publish.yml` from that tag using its matching candidate run.
  The protected npm environment rejects branch-based publication. Stage and approve
  six platform payloads before the root launcher, then verify registry integrity,
  provenance and an isolated public install. Browser/2FA authorization remains a
  human step; an old login success does not prove the current npm session is valid.
- **Authentication failures must keep skill listing responsive.** A
  repeated `invalid_token` from a Codex Apps or MCP worker can leave the app-server
  consuming CPU while a skills resolver waits. Treat an auth-required startup failure
  as terminal for the current runtime, stop reconnect attempts with unchanged
  credentials, and bound `skills/list` so healthy and local skills remain available.
  Cover both the circuit breaker and the public app-server/TUI request path; recovery
  must come from a fresh authenticated runtime or explicit credential change.

These corrections are retained in Coordinator decision
`usage-rebuild-release-prevention-v2`; test and delivery receipts remain there.
