# DEC-CODEX-015: Stable upstream 0.149.1 integration

## Evidence

On 2026-08-24, the official `openai/codex` release listing identified
`rust-v0.149.1` as the latest non-alpha release. Its annotated tag object is
`980a6d12110b110d29ec13bdcbe14011100b3566` and resolves to
`ff29a44391deccde0aba0f8390337d7f3c319ea4`.

The release is a five-commit patch line from the 0.149 release parent. It adds
bounded retained-image accounting for remote compaction, classifies detached
memory requests as memory consolidation, propagates an optional exec/SDK
thread source for newly created or forked threads, and updates the official
version. It changes no dependency, Rust toolchain, V8 pin, Bazel version,
license, notice, app-server protocol shape, or TUI snapshot.

## Options considered

- Staying on `rust-v0.149.0` would omit current stable fixes.
- Upstream `main` and `rust-v0.150.0-alpha.*` contain newer work but do not
  provide the user-selected stable release boundary.
- Replaying only the tree delta would produce equivalent files but discard the
  official release parent and make future ancestry audits harder.
- A two-parent merge retains the official tag and all downstream commits while
  allowing the sole textual conflict, the workspace version, to be resolved
  truthfully as `0.149.1+multi.1`.

## Compatibility boundaries

The additive exec `--thread-source` option defaults to `user`, affects only new
or forked threads, and does not rewrite resumed rollout sources. Remote image
budgeting remains an under-development, default-off feature; when enabled it
keeps the existing 64,000-token retained-history budget and treats each image
atomically at no more than the existing 10,000-token image estimate. The
downstream account lease, usage request chain, repository attribution,
Guardian, MCP/plugin, app-server, and TUI behavior remain required.

## Release gate

Regenerate config and app-server schemas and the Bazel lock, verify generation
is idempotent, run the focused upstream and downstream compatibility suites,
then run the complete repository-local test suite without DevCoordinator.
Build and checksum the canonical musl package, deploy only to `slawa`, and
verify version, account routing, usage access, MCP/plugin state, app-server
connectivity, real turn completion, and rollback readiness before moving the
canonical branches or publishing the downstream tag.

## Repository verification

The reconciled source passed the complete Cargo suite (15,765/15,765, with 29
configured skips), all 887 argument-comment lint targets, and all 279 runnable
Bazel targets (106 platform-inapplicable skips). The Bazel matrix includes the
native and Wine core/app-server aggregates. Schema generation was idempotent,
the TypeScript SDK passed 47/47 tests plus lint/build, no snapshot was pending,
and the final scoped Clippy and formatter passes completed successfully.

## Live deployment evidence

The canonical package was built from source commit
`b427a9200faecf6275bcb69eb97473ebce7c79b0` for
`x86_64-unknown-linux-musl`. The archive SHA-256 is
`493bfa6d52a6ea878ef98537c66763d2e6393ff6b92ff87cb21753244e1e50f3`.
Its installed Codex binary SHA-256 is
`e2fd045ef9f5d4ad8e3b97e180104b54603b9aa5b2238929959a8f1cefdd54de`,
and its code-mode-host SHA-256 is
`df6d6d33953ab1413a9e20d077872ec2e646987a533d1294f20f17efb422a58d`.

The software-owned delivery workflow activated deployment
`b61efea1-6b63-4432-a70a-863e7cb54578` for `slawa` only. The launcher and the
running Unix app-server both resolve to `0.149.1+multi.1`. The preserved exact
rollback release is `0.149.0+multi.7`; its Codex and code-mode-host SHA-256
values are `1dcfc6fb1717faf204a62985aaf241182aec50f5fea6e8fbb3ab2f258d1925ed`
and `4b0361ebcc4e8f5fca6547cb905691b870c174d2010d0d59532709d242a55da6`.

Live verification as `slawa` proved account and usage doctors healthy, three
authenticated ChatGPT profiles present, automatic higher-first selection
enabled, every account priority equal to `1000`, current service limits readable,
configured MCP servers visible, and a fresh-process real turn complete without
the former stale-rate-limit failure. The advanced current-repository usage
report returned its complete versioned coverage, token, tool, timing,
classification, participation, and formula dimensions. The app-server control
probe reported matching CLI and server versions. The downstream release tag is
`v0.149.1-multi.1`.
