# Local usage-v2 API

The app-server exposes a read-only JSON-RPC method named exactly
`localUsage/summary`. Token headlines read a compact immutable publication;
rich reviews use the dedicated `usage-v2` journal and pinned operation snapshot.
A foreground request never opens a retired SQLite database, starts a rebuild,
or requests an account credential.

`initialize.result.localUsageAccounting` advertises:

```json
{
  "version": 1,
  "supportedVersions": [1],
  "methods": ["localUsage/summary", "localUsage/repositoryIdentity", "localUsage/bindingTransition", "localUsage/bindingStatus"],
  "storage": "usage-v2",
  "supportsWorkContextBindings": true,
  "supportsWorkstreamOutcomes": true,
  "supportsTokenHeadlines": true,
  "snapshotRetentionMs": 5000
}
```

Clients select `contractVersion: 1` on each request. Omission selects version 1;
other versions fail with invalid params. This extension follows normal
app-server initialization and transport authorization.
Explicit declarations can be supplied on each request without a sidecar. This
capability does not claim every recorded operation is bound.

```json
{
  "id": 2,
  "method": "localUsage/summary",
  "params": {
    "contractVersion": 1,
    "repositoryId": "opaque-producer-repository-id",
    "fromAtMs": 1000,
    "toAtMs": 2000,
    "includeReview": true,
    "recordLimit": 20,
    "operationLimit": 20
  }
}
```

`accountId`, `repositoryId`, and `threadId` restrict recorded identities; combined
filters intersect. With no identity filter, the summary is global. Times are UTC
milliseconds, start inclusive and end exclusive. Supply both time bounds or
neither. When `supportsTokenHeadlines` is advertised, `includeReview: false`
with no requested detail pages selects `report.projection: "token_headline"`.
It returns provider token components and totals at the declared published
prefix for finite or unbounded windows. It skips operation joins and rich
review calculations. Counts, activity, attribution, cost, context and timing
are explicitly deferred: `report.counts` and `summary.counts` are null,
`review` is null, and the corresponding coverage dimensions say `deferred`.
Token coverage is independent of those deferred dimensions. A publication
backlog appears in snapshot freshness and pending entries; it does not make
measurements at the published prefix inexact. A boundary bisecting a retired
aggregate returns unavailable rather than an invented exact total.
Reviews and detail pages require finite `fromAtMs` and `toAtMs` bounds.
Combined identity filters also require a finite range.
`schemaVersion`, `repositoryKey`, `fromAt`, and `toAt` are accepted aliases for
`contractVersion`, `repositoryId`, `fromAtMs`, and `toAtMs` respectively. Non-null
`worktreeKeys` is rejected because this producer has no worktree-selection
source. The legacy `includeUnassigned` flag has no effect without that selector.
`workstreamId` selects only `review.outcomes`: totals and rows include that
stream plus explicitly unattributed unknown-stream work. Known other streams
are excluded. Repository `report`, native `summary`, and top-level review totals
retain their repository scope. An outcome without a declared stream remains
unattributed in a stream-filtered review.

The result keeps the existing `generatedAt`, `report`, and `snapshot` envelope:

```text
{
  contractVersion: 1,
  source: "usage-v2",
  generatedAt: <UTC milliseconds>,
  snapshot: {
    sourceWatermark, generatedAt, generation, watermark, journalWatermark,
    watermarkAtMs, updatedAtMs, freshness, pendingEntries, pendingCorrections,
    refreshId: null
  },
  report: {
    schemaVersion: 1, kind: "usageSummary", databaseSchemaVersion, taxonomyVersion,
    scope, timeRange, coverage, counts, providerTokens, providerTokensByActivity,
    workBindings, filters
  },
  summary: <native rich summary or token headline>,
  review: null | { coverage, totals, activities, time, tools, semantics, outcomes },
  observations: null | [<content-free UsageRecord>],
  nextRecordsCursor: null | { observedAtMs, id },
  operations: null | [<content-free ReviewOperation>],
  nextOperationsCursor: null | { observedAtMs, id },
  workContexts: [<explicit contexts with scoped operation evidence>],
  operationBindings: [<explicit operationId to workContextId declarations>]
}
```

`databaseSchemaVersion` is the usage-v2 manifest format version, retained under
the older field name for compatibility. Data within a response comes from one
pinned publication. Headline and rich views have distinct opaque snapshot
identities; clients must not interchange them.
Global `providerTokens` use the bucket `all`; repository-scoped results use the
requested repository ID. A global response does not enumerate repositories or
allocate its aggregate to them. Consumers needing one repository use its
explicit producer ID. This method does not resolve raw filesystem paths.

Rich readers include a complete bounded journal tail or leave it out as a whole
and report pending entries. Headlines are published-only and disclose the
unapplied backlog. Retention and missing facts remain visible in coverage.
Provider total tokens are never inferred from components. Cached input and
reasoning output are subsets. Corrected input, cached input, output and reasoning
components are signed integers, including their `measuredTokens`/`exactTokens`
values; provider total and cache-write tokens remain unsigned.

Execution interval sums appear only as diagnostics and are not elapsed wall
time. Rich `elapsedExecutionMs` measures the clipped union of captured execution
intervals. `activeAgentMs` unions intervals per agent/thread before summing
across agents. Total, attributed, unattributed, workstream and outcome groups
each use their own operation set: overlapping group unions are never added or
subtracted. Retired detail and missing endpoints contribute explicit unknowns.
Observed zeros remain exact zero; empty observations remain unobserved.
Request-to-delivery duration remains unknown without delivery lifecycle facts.

Cost estimates retain pricing provenance; absent trusted rates remain unknown.
Cost observation counts are distinct from unique request counts. Unknown
request counts are null, not copied from observation counts. Optional actual
billing is separate from API-equivalent estimates; retained pooled billing
does not claim exactness when individual availability is no longer known.
The API omits prompts, command arguments, tool output,
credentials, raw repository paths/remotes, and operating-system process IDs.
Storage errors return a fixed message without private paths.

Detail pages are opt-in, with zero as the default and at most 50 rows per page;
the complete encoded response is limited to 2 MiB. A continuation supplies its
cursor and `expectedSnapshot` equal to the prior `snapshot.sourceWatermark`.
The producer retains at most four rich views and sixteen headline views,
including active reads and reservations. A successful response renews its view
for five seconds; active reads cannot expire while producing a response. Global,
account, repository, and thread queries can reuse the same `expectedSnapshot`
while publication/capture continues. The journal watermark is preserved. An
expired view fails explicitly and the caller starts a new query. Full capacity
returns retryable `snapshot_capacity_busy` rather than evicting a valid view.
Omitting `expectedSnapshot` opens a fresh view. Each rich view caches up to eight
identical scoped responses within an 8 MiB byte bound for warm IPC retries.
An idle cache-owned expiry timer releases the file pins; it does not perform
capture, reviews, or work scheduling. Continuations repeat scope/time filters.
Headline and rich requests have independent bounded admission pools, so an
expensive rich query cannot consume the headline slots. Identical in-flight
requests share one source read.

The built-in `usage_stats` tool also supports an all-history global
`performance_review`. That request reads a completed background publication;
it does not open the current operation history in the foreground. Its source
generation, manifest watermark and included journal watermark identify the
facts in the report. New capture is disclosed as pending entries. Pagination
keeps that completed report and its source metadata fixed. Before the first
publication exists, the tool returns an explicit retryable pending result.
Finite and identity-filtered reviews retain their requested scope.

The global analytics publisher is separate from Coordinator review policy.
It uses bounded private database resources, publishes only complete results,
keeps the previous completed report on failure, and is interruptible during
shutdown. Invalid derived caches are rebuilt from canonical observations;
retired usage formats are not imported or migrated.

`workContextBindings` is an optional request array, bounded to 256 declarations
and 256 distinct contexts. Headlines validate declaration shape but defer
identity resolution and return no binding evidence. Rich requests resolve the
declarations below. Every entry has this strict versioned shape:

```json
{"schemaVersion":1,"operationId":"recorded-operation-id","capturedAtMs":2000,"workContext":{"schemaVersion":1,"id":"C1","repositoryId":"coordinator-repo","nativeProjectId":"project-one","workstreamId":"stream-one","outcomeId":"outcome-one","threadId":"recorded-thread","turnId":"recorded-turn","experimentRef":"experiment-one@1"}}
```

`nativeProjectId` is canonical; `projectId` remains a backward input alias.
Snake-case aliases are also accepted. The context's `repositoryId` is the
Coordinator authority repository ID; the summary filter is the separate producer
repository key. The collector validates its authority scope before sending
declarations. The producer validates recorded operation repository against the
summary key, and checks supplied thread/turn against recorded facts when known.
These repository namespaces are never equated. Contexts are declared external
identities, not inferred translations of the producer's repository or thread ID.
Context IDs must have one consistent definition, and an operation may have only
one consistent binding. IDs allow ASCII letters, numbers, hyphens, underscores,
periods, and colons, with a maximum length of 256; `experimentRef` also permits
`@`. All schemas must be version 1,
capture times must be nonnegative, and unknown fields or conflicting duplicate
bindings fail closed with a fixed error that omits private content and paths.
The endpoint never persists request declarations into canonical usage. Optional
`usage-v2/work-contexts.jsonl` remains a compatibility input, using the same
entry schema, bounded to 512 KiB and 256 nonempty lines. Only identical consistent
declarations merge; conflicts fail closed and never overwrite either source.
The API never creates/edits that file. Writers should use atomic replacement.

The response joins declarations to actual scoped facts using only exact
`operation_id` equality. It returns those `operationBindings` and `workContexts`,
and groups measured effort under `review.outcomes.rows` by declared outcome ID.
An outcome row also carries its declared `workstreamId`. Contexts without an
outcome ID, missing or retired operation facts, response-only token links, and
operations without bindings stay unattributed. Binding declarations alone never
create operations, tokens, time, or delivery receipts. Without exceptional
declarations, sparse assignment events provide the native binding source below.
Missing assignment evidence remains unattributed.

`report.workBindings` identifies the declaration basis, bound/unknown operation
counts, matched/unmatched declaration counts, and a readable source revision.
Content hashes remain private. A retained view pins compatibility input; request
declarations are an explicit overlay and are included in the response-cache key,
not written into usage or guessed from a snapshot token. Fresh views observe
later compatibility changes. The
native `summary` remains the underlying storage reader's view; composite binding
coverage belongs to `report.workBindings` and `review.outcomes`.

Outcome coverage is `unattributed`, `partial`, or `complete` according to the
measured facts that could be explicitly joined. These are accounting coverage
states, not proof of a delivered outcome. Missing measurements stay unknown.
The retained source fixture is
`codex-rs/app-server/tests/fixtures/local_usage/work-contexts.jsonl`; the RPC tests
cover compatibility input, request-only full contexts, scope filtering, exact
identity conflicts, journal late facts, cache expiry, and isolated IPC latency.
`codex-rs/app-server/tests/fixtures/local_usage/bound-summary.json` retains an
actual app-server RPC response from isolated synthetic test data. Its capture
and revision timestamps are test-run metadata, not production usage evidence.

`localUsage/repositoryIdentity` accepts `{contractVersion: 1, path: <local absolute
path>}` and uses the same read-only native identity helper as capture. It returns
`{contractVersion: 1, source: "usage-v2", repositoryId: <opaque ID or null>,
formatVersion: 3}`. The path is private input and never appears in response or
error. Linked worktrees share the validated Git common-directory identity.

For real cross-component acceptance, `CODEX_USAGE_API_FIXTURE_DIR` exports an
isolated synthetic store without a sidecar, plus private `fixture-info.json`,
`bound-summary.json`, and `latency.json`. `CODEX_USAGE_V2_FIXTURE_REPOSITORY`
selects its authorized source repository path. Fixture timestamps are near the
test run so live retention cannot retire them during acceptance. Copy this
fixture into a new isolated home before starting a companion app-server.

## Sparse assignment control

`localUsage/bindingTransition` changes explicitly declared work for a loaded
thread. Start or resume that thread first. Send one declaration per actual job
change; the same outcome keeps its binding across operations, retries, compaction
and account changes. Reading a task, repository identity, phase or token totals
never creates an assignment. The built-in agent tool `usage_work` provides
`set`, `clear` and `status` for its invocation's current thread, using the same
capture queue as these RPC methods.

Read `localUsage/bindingStatus` with `threadId` and optional `version: 1` before
changing an unknown binding. It returns `{version: 1, revision, transition}`.
Revision zero and an explicit null transition mean no declaration was captured.
Otherwise the typed transition contains `threadId`, nullable `nativeProjectId`,
`workstreamId` and `outcomeId`, `effectiveCaptureSequence`, `capturedAtMs`,
`source` and `authority`. Nullable output fields are always present. A captured
clear has a nonzero revision and a null project.

Version 1 transition requests contain `version: 1`, `threadId`,
`expectedRevision`, and optional `nativeProjectId`, `workstreamId` and
`outcomeId`. Set the project ID to declare work. A null or omitted project
explicitly clears the binding; workstream and outcome must then be null or
omitted. Identifiers contain 1–128 ASCII letters, digits, hyphens, underscores,
periods or colons. Callers cannot supply source or authority: the runtime records
new declarations as `agent_declared`. Those provenance labels neither verify a
Coordinator task nor prove delivery.

The content-free `usage-v2/work-bindings.jsonl` journal retains one event per
accepted switch. Its capture boundary is the next canonical usage sequence,
assigned after preceding observations in the same capture queue are durable.
Supply the current revision as `expectedRevision`. An identical retry of the
latest accepted request and an unchanged binding both return the existing
receipt without another event; a stale conflicting revision fails. The response
contains the actual `revision`, `effectiveCaptureSequence`, `capturedAtMs` and
`version: 1`. An unavailable or pending response does not acknowledge a switch;
inspect status before retrying.

Native performance reports join an operation's first captured sequence to the
latest applicable switch for its thread. Later facts retain that first binding.
Retries follow an explicit, same-thread predecessor chain; absent or retired
predecessors remain unattributed. Exceptional request declarations override
sparse assignment for those exact operation IDs without double counting.
`operationBindings` continues to contain only exceptional declarations; native
bindings are represented by sparse contexts and aggregate counts. No assignment
metadata is added to model requests, prompts, or individual usage records.

Initialize advertises both binding methods alongside `localUsage/summary` and
`localUsage/repositoryIdentity`. The read-only local deployment verifier accepts
the historical two-method capability for rollback and the exact four-method
capability. A plan with `requireSparseBindings: true` requires all four; the
default is false. Its `usageSparseControlCapability` proof records the advertised
contract without changing a binding. Actual switch behavior, retention and scale
still require their separate acceptance evidence.
