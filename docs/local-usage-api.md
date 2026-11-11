# Local usage-v2 API

The app-server exposes a read-only JSON-RPC method named exactly
`localUsage/summary`. It reads the dedicated `usage-v2` journal and immutable
snapshot through `codex_usage::UsageReader`. It never opens or migrates a retired
SQLite usage database, starts a rebuild, or requests an account credential.

`initialize.result.localUsageAccounting` advertises:

```json
{
  "version": 1,
  "supportedVersions": [1],
  "methods": ["localUsage/summary", "localUsage/repositoryIdentity"],
  "storage": "usage-v2",
  "supportsWorkContextBindings": true,
  "supportsWorkstreamOutcomes": true,
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
neither. Unbounded summaries use precomputed metadata only: provider totals and
performance dimensions are unavailable there, with empty `providerTokens` and
explicit coverage. They never trigger a full-history performance query. Reviews
and detail pages require finite `fromAtMs` and `toAtMs` bounds.
Combined identity filters also require a finite range. If unpublished token
corrections make the metadata fast path unsafe, an unbounded request returns a
source-unavailable error (`-32603`, data reason `metadata_corrections_pending`,
coverage `unavailable`); it does not infer a caller error or invent totals.
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
  summary: <native UsageReader.summary>,
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
the older field name for compatibility. Native summary/review data, token
categories, coverage, and unknown measurements come from the same pinned reader.
Global `providerTokens` use the bucket `all`; repository-scoped results use the
requested repository ID. A global response does not enumerate repositories or
allocate its aggregate to them. Consumers needing one repository use its
explicit producer ID. This method does not resolve raw filesystem paths.

The reader includes a complete bounded journal tail or leaves it out as a whole
and reports stale data with pending entries. Retention and missing facts remain
visible in coverage. Provider total tokens are never inferred from token
components. Cached input and reasoning output are subsets. Execution interval
sums appear only as `report.measuredExecutionIntervalSumMs` and must not be
interpreted as elapsed wall time. Review/outcome `elapsedExecutionMs` remains
unknown (`measured: 0`, `exact: null`, with missing interval counts), rather than
claiming an interval sum is a wall-clock union. Delivery and active-agent time
also remain unknown.
The API omits prompts, command arguments, tool output,
credentials, raw repository paths/remotes, and operating-system process IDs.
Storage errors return a fixed message without private paths.

Detail pages are opt-in, with zero as the default and at most 50 rows per page;
the complete encoded response is limited to 2 MiB. A continuation supplies its
cursor and `expectedSnapshot` equal to the prior `snapshot.sourceWatermark`.
The producer retains at most four actual immutable readers for up to five
seconds, including captured tail records and compatibility declarations. Global,
account, repository, and thread queries can reuse the same `expectedSnapshot`
while publication/capture continues. The journal watermark is preserved. An
expired or evicted view fails explicitly and the caller starts a new query.
Omitting `expectedSnapshot` opens a fresh view. Each view also caches up to eight
identical scoped responses within an 8 MiB byte bound for warm IPC retries.
An idle cache-owned expiry timer releases the file pins; it does not perform
capture, reviews, or work scheduling. Continuations repeat scope/time filters.

`workContextBindings` is an optional request array, bounded to 256 declarations
and 256 distinct contexts. Every entry has this strict versioned shape:

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

`localUsage/bindingTransition` is an out-of-band control request, sent only when
the explicitly assigned job changes. Version 1 accepts `nativeProjectId`,
`threadId`, optional `workstreamId` and `outcomeId`, `transitionSequence`,
`source`, and `authority`. Null project/work IDs explicitly clear the assignment.
Identifiers contain at most 128 ASCII letters, digits, hyphens, underscores,
periods or colons. Source and authority describe the producer's assertion;
neither verifies a Coordinator task or proves delivery. Reading a task, repository
identity, phase and token totals never create an assignment.

The content-free `usage-v2/work-bindings.jsonl` journal retains one event per
accepted switch. Its capture boundary is the next canonical usage sequence,
assigned under the same writer lock as observations. An identical retry is
idempotent. An older or conflicting sequence for that thread is rejected.
Runtime producers must serialize the switch with their capture queue before
calling this storage boundary; synchronous external producers already satisfy
that ordering through the journal writer lock.

Native performance reports join an operation's first captured sequence to the
latest applicable switch for its thread. Later facts retain that first binding.
Retries follow an explicit, same-thread predecessor chain; absent or retired
predecessors remain unattributed. Exceptional request declarations override
sparse assignment for those exact operation IDs without double counting.
`operationBindings` continues to contain only exceptional declarations; native
bindings are represented by sparse contexts and aggregate counts. No assignment
metadata is added to model requests, prompts, or individual usage records.

This protocol/storage stage remains preliminary until the runtime queue and
agent control surface are integrated and verified. Retention, large-history
indexing and binding-specific unavailable coverage also require the follow-up
acceptance review; the new control method is not yet advertised in initialize.
