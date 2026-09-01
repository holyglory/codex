# Decision History

## Direction

Confirmed user intent is to maintain an upstream-compatible Codex CLI fork,
not a wrapper, with first-class multi-account behavior (DEC-CODEX-001) and
required, content-free local accounting that explains token and tool usage per
chat and repository (DEC-CODEX-002). Compatibility, truthful measurement, and
inspectable provenance take priority over apparently precise but unsupported
attribution (DEC-CODEX-001, DEC-CODEX-002). Development tracks a pinned,
verified stable upstream release rather than a moving branch or alpha tag
(DEC-CODEX-003, DEC-CODEX-015). The machine's local accounts are one operator
boundary, so repository collaboration uses one equal-rights group rather than
per-account ACL exceptions (DEC-CODEX-007). Testing and build verification for
this repository use repository-local workflows; DevCoordinator is not used unless
the user explicitly reverses that decision (DEC-CODEX-008). Automatic failover
uses only locally managed ChatGPT OAuth profiles (DEC-CODEX-009), and the first
deployment is limited to the `slawa` Unix account (DEC-CODEX-010). Unknown or
stale automatic-selection capacity is resolved through bounded, read-only,
profile-leased service probes before model work begins (DEC-CODEX-011). An open
subscription window remains eligible without purchased top-up credits unless
the service explicitly reports a reached condition (DEC-CODEX-012). Account
priority is a descending drain rank: higher numbers are consumed before lower
numbers, with `1000` as the neutral default (DEC-CODEX-013). Complete approved
usage facts and safe account-routing controls are available to both the operator
and in-product agents through bounded structured surfaces (DEC-CODEX-014).

### DEC-CODEX-001

- **Decision:** Build a maintained downstream fork of the open-source Codex CLI
  that preserves the `codex` executable and upstream interfaces while adding
  multi-account functionality through shared core changes and additive
  capability-gated RPCs.
- **Why:** A real fork was selected over a wrapper, proxy, or parallel
  executable because only the fork can provide profile-aware authentication,
  turn-boundary account snapshots, CLI/TUI integration, and stock app-server
  compatibility end to end. Wrappers and proxies would duplicate or bypass
  upstream authentication and lifecycle behavior.
- [Details](DecisionDetails/DEC-CODEX-001.md)

### DEC-CODEX-002

- **Decision:** Add a required private SQLite usage database per `CODEX_HOME`,
  capture provider-native token and tool facts in the shared core path, and use
  a short `usage_activity` tool plus runtime evidence to classify work with
  explicit provenance and accounting overhead.
- **Why:** Core local capture was selected over optional OTel-only export, log
  scraping, or aggregate account reports because the user requires durable
  per-chat and per-repository detail. Activity-boundary declarations plus
  deterministic evidence were selected over heuristic-only classification
  because they are inspectable and correctable; unsupported token splitting
  remains mixed or unknown.
- [Details](DecisionDetails/DEC-CODEX-002.md)

### DEC-CODEX-003

- **Decision:** Pin the initial fork baseline to the stable upstream tag
  `rust-v0.149.0` at commit `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`.
- **Why:** The current stable tag was selected over upstream `main`, an alpha
  tag, or the older handover example `rust-v0.148.0` so implementation has a
  reproducible compatibility target without starting one release behind.
- [Details](DecisionDetails/DEC-CODEX-003.md)

### DEC-CODEX-004

- **Decision:** Preserve upstream `/usage` account-activity behavior, extend
  its menu with local statistics, and expose new app-server operations through
  distinct `accountProfile*` and `localUsage*` singular-resource namespaces.
- **Why:** Upstream v0.149.0 already owns `/usage daily|weekly|cumulative` and
  `account/usage/read`. Extending the menu and using collision-free additive
  RPCs preserves stock behavior; repurposing those surfaces would violate the
  compatibility contract.
- [Details](DecisionDetails/DEC-CODEX-004.md)

### DEC-CODEX-005

- **Decision:** Add a parallel presence-preserving provider-usage model and
  treat the durable model-request start as the covering start for
  provider-hosted tools that execute before the client can observe them.
- **Why:** Upstream v0.149.0 collapses some absent usage fields to zero and
  discards some failed/image usage, while hosted tools cannot have a truthful
  client-side pre-execution event. Extending compatibility types or inventing
  tool starts would produce false statistics; parallel exact observations and
  explicit unknown/covering provenance preserve truthfulness.
- [Details](DecisionDetails/DEC-CODEX-005.md)

### DEC-CODEX-006

- **Decision:** Reject an explicit `--account` pin when an environment, host,
  workload-identity, or externally managed auth source would override the
  selected local profile.
- **Why:** Letting external auth win would make the pin dishonest, while
  silently ignoring externally owned credentials could break automation and
  trust boundaries. A deterministic pre-request conflict is explicit,
  reversible, and preserves both ownership models.
- [Details](DecisionDetails/DEC-CODEX-006.md)

### DEC-CODEX-007

- **Decision:** Give every participating local account equal owner-equivalent
  access to the entire checkout through one shared group, including `.git`,
  `.state`, build targets, and caches; use inherited group permissions and no
  named user ACL entries.
- **Why:** A shared group was selected over owner-private modes, excluded
  internal directories, or accumulating per-user ACLs because all machine
  accounts belong to one operator and must collaborate on the same checkout.
  The earlier exclusions caused cross-account build and ledger failures.
- [Details](DecisionDetails/DEC-CODEX-007.md)

### DEC-CODEX-008

- **Decision:** Stop using DevCoordinator for `/home/CodexMulti`; use
  repository-local test, build, schema, and verification workflows unless the
  user explicitly reverses this decision.
- **Why:** Repository-local workflows were selected over further Coordinator
  planning, submission, or follow-up because the Coordinator handoff duplicated
  resource-heavy targets, exhausted disk capacity, failed to cancel cleanly,
  and later could not produce a valid replacement plan even though the retained
  individual suites were already green.
- [Details](DecisionDetails/DEC-CODEX-008.md)

### DEC-CODEX-009

- **Decision:** Restrict automatic account selection and failover to locally
  managed ChatGPT OAuth profiles; all other authentication modes remain
  manually selectable or pinnable.
- **Why:** ChatGPT-only automatic selection was selected over crossing into
  PAT, API-key, Bedrock, Agent Identity, header-backed, or externally managed
  auth because those modes may have separate billing or ownership semantics.
  Manual selection preserves their usefulness without creating surprise cost
  or authority changes.
- [Details](DecisionDetails/DEC-CODEX-009.md)

### DEC-CODEX-010

- **Decision:** Deploy `0.149.0+multi.1` only for the `slawa` Unix account;
  leave the `holyglory`, `holygloryTT`, and `axel` installations unchanged.
- **Why:** A one-user deployment was selected over the former four-user rollout
  because the user explicitly limited the target to `slawa`. It provides a
  bounded real installation and rollback surface while preserving the other
  three working installations.
- [Details](DecisionDetails/DEC-CODEX-010.md)

### DEC-CODEX-011

- **Decision:** When automatic selection is enabled, refresh unknown or stale
  managed-ChatGPT profile limits through bounded read-only service probes before
  selecting the immutable turn account lease.
- **Why:** Selection-time probes were chosen over treating unknown capacity as
  eligible, aborting every turn, or persisting quota snapshots across processes.
  They establish current service evidence without starting model/tool work;
  failed probes remain unknown and selection stays fail-closed.
- [Details](DecisionDetails/DEC-CODEX-011.md)

### DEC-CODEX-012

- **Decision:** Treat explicit reached types and subscription/spend windows as
  authoritative capacity evidence; use depleted top-up credits as a reached
  fallback only when no usable subscription or positive-credit evidence exists.
- **Why:** Open subscription capacity was selected over interpreting
  `has_credits=false` as total exhaustion because live Pro responses use that
  flag for optional purchased credits while independently reporting an open
  subscription window. The prior precedence rejected valid accounts.
- [Details](DecisionDetails/DEC-CODEX-012.md)

### DEC-CODEX-013

- **Decision:** Interpret account priority as a descending drain rank: select
  the highest numeric eligible tier, retain the current account only within
  that tier, and give new profiles priority `1000` by default.
- **Why:** Descending rank was selected over the prior lower-first/current-first
  behavior or implicit insertion order because the user wants smaller values
  preserved until higher-ranked capacity is exhausted. Equal-rank retention
  avoids needless turn-to-turn switching while remaining deterministic.
- [Details](DecisionDetails/DEC-CODEX-013.md)

### DEC-CODEX-014

- **Decision:** Expose the complete approved local-usage report and safe
  account-routing metadata through versioned human interfaces and bounded
  in-product agent tools; keep credentials and prohibited content excluded.
- **Why:** Structured CLI/TUI/app-server and paginated agent access was selected
  over shell-only JSON, direct SQLite access, or reduced aggregates because all
  supported callers need truthful coverage, provenance, classification,
  timing, concurrency, and routing control without creating a network service
  or weakening the existing private `CODEX_HOME` boundary.
- [Details](DecisionDetails/DEC-CODEX-014.md)

### DEC-CODEX-015

- **Decision:** Advance the maintained fork from `rust-v0.149.0` to the stable
  upstream `rust-v0.149.1` tag at commit
  `ff29a44391deccde0aba0f8390337d7f3c319ea4`, preserving the official tag as a
  merge parent and publishing the downstream build as `0.149.1+multi.1`.
- **Why:** The official stable patch was selected over upstream `main`, the
  newer alpha line, or remaining on 0.149.0 because it fixes compaction,
  memory classification, and exec thread-source behavior at a reproducible
  compatibility boundary. A two-parent merge preserves both release ancestry
  and downstream history; copying the tree delta or claiming plain 0.149.1
  would lose provenance or misstate the modified binary.
- [Details](DecisionDetails/DEC-CODEX-015.md)
