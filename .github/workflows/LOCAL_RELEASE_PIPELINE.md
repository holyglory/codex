# Local compilation, native verification, and npm publication

This is the preferred path for future CodexMulti releases. It separates expensive
compilation from target-platform execution and from registry publication. An
already accepted release continues on its frozen source and workflow; adopting
this path must not cancel, relabel, or replace that release's running jobs.

## Where work runs

| Work | Default location | Required verification |
| --- | --- | --- |
| Linux x64 musl compilation | Persistent Linux build host | Linux x64 execution of the resulting tests and package |
| Linux ARM64 musl compilation | Cross-compile on the same Linux host | Native GitHub ARM64 runner executes the transferred tests and package |
| Windows x64 and ARM64 compilation | Existing private Windows runner and caches | Preserve x64 runtime checks and the explicitly recorded ARM64 cross-compilation boundary |
| macOS Intel and Apple Silicon compilation | Existing hosted Mac runners | Matching macOS runtime checks |
| Seven npm tarballs | Persistent Linux build host | Independent archive and installation verification in GitHub |
| Provenance and npm staging | Existing GitHub trusted-publishing environment | Exact source, bytes, successful checks, and npm's external human approval |

There is no configured local Mac builder. A future local Mac may replace the
hosted compilation step after its SDK, signing tools, cache ownership, and both
architecture verification paths are qualified. Linux-to-macOS cross-compilation
is not the default: this repository uses Apple SDK tools including `xcrun`,
`install_name_tool`, and `codesign` for native resources.

Docker on an x86-64 host does not provide native ARM64 execution. QEMU may help
diagnosis, but an emulated result must not be presented as the required native
ARM64 release result.

## The artifact boundary

1. Freeze the product commit and a fresh downstream version. Never move a
   released tag or replace already published bytes under its version.
2. Resolve Rust from the checked-out release declaration. Validate `rustc` and
   Cargo before compiling; a machine's default or a permanently hardcoded Rust
   version is not the release toolchain.
3. Prepare target-specific Zig/C/C++ libraries and a matching V8 archive/binding
   pair. ARM64 must not silently use a host x64 `musl-gcc`, library, or header
   search path. Preserve Windows C++ unwinding flags for both architectures.
4. Compile release executables and a Nextest archive locally. Include integration
   binaries, generated runtime resources, and the exact test configuration.
5. Seal a compiled-input manifest with source/native trees, version, target,
   Rust/Nextest versions, file sizes, and SHA-256 digests. Its status is
   `compiled`, with `runtime_executed: false`.
6. Transfer the immutable bundle through the existing owned GitHub artifact
   boundary. A descriptor outside the archive binds its URL, size, archive
   digest, and manifest digest.
7. The native runner checks out the same product revision for fixtures,
   validates every input, checks its actual OS/architecture, remaps workspace
   paths, and runs the archived tests. It must not invoke a product build.
8. Run the version/help smoke checks against the same prebuilt release package.
   Capture test exit results and log digests in a separate verification receipt.
9. Only a successful verification receipt bound to the compiled manifest may
   produce the existing successful native handoff consumed by npm assembly.

Do not mark an archive tested merely because it compiled or downloaded. A failed
test, missing fixture, wrong architecture, absent binary, or source/digest
mismatch keeps that target unqualified. Keep the existing test assertions and
deadlines; do not turn missing execution into a skipped release gate.

## Local native entry point

`scripts/local_native_release.py` owns compilation, native verification, and
sealing as separate operations. `scripts/prepare_local_musl.py` owns target
dependency preparation in a user-owned persistent cache. The native GitHub
consumer is `downstream-prebuilt-native.yml`.

Use the command's `--help` for the exact argument contract. A local compile
operation produces a directory containing `build-manifest.json`, the canonical
release package, the Nextest archive, V8 provenance, and test configuration.
The verifier consumes those bytes and emits `verification.json`; sealing binds
the two records. Neither verification nor sealing is permission to rebuild.

The Nextest archive and runtime tool version must agree. Use target-only
selection on the target runner; host-only tests belong on the build machine.
Keep the V8/CppGC ignored-test gate separate from ordinary product regressions
so enabling that gate does not accidentally run unrelated ignored scale tests.

Run the commands below as finite Coordinator checks in an accepted graph, from
an immutable checkout. `PRODUCT_SOURCE` is the product checkout; `HELPER_SOURCE`
is the clean reviewed pipeline checkout. The two commits and trees are recorded
separately. Install the source-declared Rust toolchain and target standard library,
the source-pinned Bazel version, Zig 0.14.0, Nextest, just, Python 3.12+, Perl,
make, a host C compiler, pkg-config, and a target-capable strip tool first.

The existing V8 producer supports local cross-compilation. For ARM64, use
`TARGET=aarch64-unknown-linux-musl` and `BAZEL_PLATFORM=linux_arm64_musl`; for x64,
use `x86_64-unknown-linux-musl` and `linux_amd64_musl`. Set `SOURCE_VERSION` from
the product Cargo manifest, `RUNNER_TEMP` and `V8_HANDOFF_DIR` to unused per-run
directories, and persistent `BAZEL_OUTPUT_USER_ROOT`, `BAZEL_REPOSITORY_CACHE`,
and `BAZEL_REPO_CONTENTS_CACHE` paths. Then, in the product checkout:

```sh
python3 .github/scripts/bootstrap_bazel_distdir.py \
  --workspace "$PRODUCT_SOURCE" --distdir "$PERSISTENT_BAZEL_DISTDIR"
env -u BUILDBUDDY_API_KEY BAZEL_CONFIG=ci \
  bash .github/scripts/downstream-unix-native.sh source-v8
```

The environment removal keeps this build local. The bootstrap helper writes the
ignored `user.bazelrc` distdir entry. The wrapper supports `BAZEL_JOBS` and
`BAZEL_OUTPUT_USER_ROOT`; it does not read `BAZEL_OUTPUT_BASE` or `BAZEL_DISTDIR`.
The `ci` configuration disables the action disk cache, so do not claim that cache
is active: the persistent output tree and repository caches provide the reuse.

After the V8 source pair is sealed:

```sh
python3 "$HELPER_SOURCE/scripts/local_native_release.py" compile \
  --source-root "$PRODUCT_SOURCE" --commit "$PRODUCT_COMMIT" --target "$TARGET" \
  --cargo-target-dir "$TARGET_CARGO_CACHE" --musl-cache "$DEPENDENCY_CACHE" \
  --zig "$ZIG_EXECUTABLE" --strip-tool "$TARGET_STRIP_EXECUTABLE" \
  --v8-handoff "$V8_HANDOFF_DIR" --output "$NEW_COMPILED_DIRECTORY"
```

Do not share a mutable target Cargo or Bazel output directory between simultaneous
builds. Declare that cache as an exclusive Coordinator resource or use separate
per-target directories. A Cargo lock wait is not compilation progress.

Create the transport with the helper, which preserves canonical relative members
and checks every file before and after bundling:

```sh
python3 "$HELPER_SOURCE/scripts/local_native_transport.py" bundle \
  --source-root "$PRODUCT_SOURCE" --commit "$PRODUCT_COMMIT" --target "$TARGET" \
  --input "$NEW_COMPILED_DIRECTORY" --manifest-sha256 "$BUILD_MANIFEST_SHA256" \
  --output "$NATIVE_BUNDLE" --metadata "$NATIVE_TRANSPORT_METADATA"
```

Retain the emitted metadata digest separately. After uploading that immutable
bundle to an owned draft-release asset, bind the returned API URL:

```sh
python3 "$HELPER_SOURCE/scripts/local_native_transport.py" descriptor \
  --bundle "$NATIVE_BUNDLE" --metadata "$NATIVE_TRANSPORT_METADATA" \
  --metadata-sha256 "$NATIVE_METADATA_SHA256" --asset-url "$UPLOADED_GITHUB_ASSET_URL" \
  --output "$NATIVE_TRANSPORT_DESCRIPTOR"
```

Neither command uploads, dispatches a job, or marks a package tested.

## GitHub verification entry points

Use the registered `downstream-candidate.yml` workflow on the reviewed pipeline
branch or immutable packaging reference. Its default mode is `local-npm`.

| `pipeline_mode` | Required mode-specific input | Work performed |
| --- | --- | --- |
| `verify-native` | `prebuilt_native_descriptor` JSON | Execute one locally compiled Linux target on matching native hardware |
| `macos-build` | No additional inputs | Build and test only the two Mac targets from the exact product revision |
| `local-npm` | `local_npm_descriptor` JSON and `release_tag` | Verify, install, attest, and retain the original seven tarballs |
| `hosted-build` | Existing handoff/producer and acceptance inputs | Explicit fallback to the previous hosted compilation pipeline |

Every mode also takes `native_source_commit`. A native descriptor contains
`target`, `url`, `size_bytes`, `sha256`, and `manifest_sha256`; the asset URL must
be the owned GitHub release-asset API URL. The helper revision is the workflow
revision and must match the local compiler's recorded helper commit and bytes.
Do not dispatch a native archive made by a different helper revision.

Use `macos-build` for the current Mac fallback while Linux compiles locally and
the existing private Windows controller builds its two targets. It does not
also start a hosted Linux build or npm assembly. Each Mac producer checks out
the product commit, so its receipt names the same source as the local builds.

Prepare a JSON file of workflow inputs, then submit it once:

```sh
gh workflow run downstream-candidate.yml --repo holyglory/codex \
  --ref "$PIPELINE_REF" --json < "$DISPATCH_INPUTS_JSON"
```

Record the returned run identity, attempt, source and input digests before
waiting. Separate target concurrency groups preserve simultaneous x64 and ARM64
verification. The two native verifier jobs cannot qualify npm publication;
publication requires the complete `local-npm` verification and final result job.

## Local npm assembly

The existing npm assembly code performs extraction, metadata generation, and
`npm pack`; it does not compile Rust or V8. Assemble only after all six native
payloads and their verification receipts are available.

```sh
python3 scripts/local_npm_release.py assemble \
  --source-commit "$PRODUCT_COMMIT" \
  --packaging-commit "$PACKAGING_COMMIT" \
  --artifacts-dir "$VERIFIED_NATIVE_INPUTS" \
  --local-acceptance "$LOCAL_ACCEPTANCE_RECORD" \
  --delivery-receipt "$QUALIFIED_DELIVERY_RECEIPT" \
  --output-dir "$NEW_CANDIDATE_DIRECTORY"
```

The output contains the seven tarballs, `npm-build-manifest.json`, native
provenance, local acceptance evidence, and an immutable candidate archive.
The local descriptor is explicitly unpublished. Upload the archive to an owned
draft release or artifact location, then bind the actual returned asset URL:

```sh
python3 scripts/local_npm_release.py descriptor \
  --candidate "$NEW_CANDIDATE_DIRECTORY/local-candidate.json" \
  --asset-url "$UPLOADED_GITHUB_ASSET_URL" \
  --output "$TRANSPORT_DESCRIPTOR"
```

`downstream-local-npm.yml` verifies the exact packaging commit and product tag,
downloads the declared asset, validates the seven original tarballs and native
receipts, and performs a fresh isolated installation. It does not recompile or
repack the candidate. Its publication artifact contains the seven original
tarballs and receipts; the input asset retains the larger native archives, so
they are not uploaded a second time. The bytes attested and staged must be the
verified bytes.

The root package pins all six platform versions. Verify OS/CPU constraints,
required executables, `LICENSE`/`NOTICE`, launcher ownership, version/help output,
and the missing-payload error path. An x64 Linux install check does not replace
the native platform checks recorded before assembly.

`downstream-npm-publish.yml` remains the sole npm staging workflow and uses the
existing `npm` environment and OIDC identity. Pass it the successful complete
candidate run and annotated product tag on the same packaging reference. Stage platform
payloads before the root launcher. Approval in npm remains an external human
action; a workflow Boolean is not evidence of that approval. After approval,
verify every public version, tag, and tarball integrity plus a fresh installation
from the public registry.

## Caches and execution ownership

- Keep Cargo, Bazel, downloaded dependencies, and compiler caches on approved
  persistent build storage. Separate incompatible targets and compiler settings;
  key reuse by actual source inputs, target, toolchain, and build profile.
- Keep per-run staging and receipts distinct from reusable caches. Never
  overwrite a sealed manifest or reuse an interrupted output as successful.
- Coordinator owns local capacity and accepted jobs. Submit independent ready
  build checks together; express only real dependencies in the graph.
- A remote workflow dispatch is a finite local operation. Persist its exact
  run/attempt identity, then observe it with the provider's read-only wait.
  Do not hold a local compiler slot for hours merely to poll a remote build.
- Preserve active jobs and completed target checkpoints. A failed platform does
  not require rebuilding successful platforms with unchanged inputs.
- No guest bridge, broker installation, reboot, unrelated service restart, or
  foreign request acknowledgement is part of release compilation.

## Recovery and evidence

An interrupted remote runner is distinct from a failed compiler or test. Retain
the exact job and logs, diagnose the reason, and retry only the failed job when
the provider permits it. If the provider refuses while sibling jobs run, wait;
do not cancel those jobs or submit duplicate work.

A disconnected observer does not mean its remote job stopped. Reconnect using
the recorded run ID before deciding whether to dispatch. An upload intent with
an uncertain result requires inspecting the exact existing asset before retrying;
do not overwrite or duplicate it.

Completed native output may be verified and retained by a later short check if
its observer graph was cancelled. Keep the original manifest and successful
individual check hashes unchanged, and record the original aggregate cancellation
in the new receipt. The cancelled graph is never relabeled as a passing graph.

The source-owned contract tests cover handoff tampering, missing platform/input
data, wrong source/architecture, archive-path validation, and absence of compile
or repack calls in verification. Real native execution remains a separate gate;
fixture tests alone cannot qualify a release or activate this pipeline on a new
builder.
