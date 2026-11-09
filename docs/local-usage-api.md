# Local usage-v2 API

The app-server exposes a read-only JSON-RPC method named exactly
`localUsage/summary`. It reads the dedicated `usage-v2` journal and immutable
snapshot through `codex_usage::UsageReader`. It never opens or migrates a retired
SQLite usage database, starts a rebuild, or requests an account credential.

With no binding sidecar, `initialize.result.localUsageAccounting` advertises:

```json
{
  "version": 1,
  "supportedVersions": [1],
  "methods": ["localUsage/summary"],
  "storage": "usage-v2",
  "supportsWorkContextBindings": false
}
```

Clients select `contractVersion: 1` on each request. Omission selects version 1;
other versions fail with invalid params. This extension follows normal
app-server initialization and transport authorization.
`supportsWorkContextBindings` is true only when a valid optional sidecar is
present at initialization. That flag does not claim every operation is bound.

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
milliseconds, start inclusive and end exclusive. Omitted bounds mean all time.
`schemaVersion`, `repositoryKey`, `fromAt`, and `toAt` are accepted aliases for
`contractVersion`, `repositoryId`, `fromAtMs`, and `toAtMs` respectively. Non-null
`worktreeKeys` is rejected because this producer has no worktree-selection
source. The legacy `includeUnassigned` flag has no effect without that selector.
Non-null `workstreamId` is also recognized and rejected: complete workstream
filtering is not supported. Consumers must mark that scoped summary unavailable;
they must not treat the repository-wide response as a workstream summary.

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
  review?: { coverage, totals, activities, time, tools, semantics, outcomes },
  observations?: [<content-free UsageRecord>],
  nextRecordsCursor: null | { observedAtMs, id },
  operations?: [<content-free ReviewOperation>],
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
components. Cached input and reasoning output are subsets. Review execution
interval sums appear as `report.measuredExecutionIntervalSumMs` and outcome
effort, and must not be interpreted as elapsed wall time or active agent time.
The review's wall-clock, delivery, and active-agent time fields remain unknown.
The API omits prompts, command arguments, tool output,
credentials, raw repository paths/remotes, and operating-system process IDs.
Storage errors return a fixed message without private paths.

Detail pages are opt-in, with zero as the default and at most 50 rows per page;
the complete encoded response is limited to 2 MiB. A continuation supplies its
cursor and `expectedSnapshot` equal to the prior `snapshot.sourceWatermark`.
If publication or capture changes that identity, the request fails and the
client restarts pagination. The API does not keep a server-side query lease.
Continuations repeat the original scope and time filters.

An optional `usage-v2/work-contexts.jsonl` supplies explicit declarations. This
read-only API never creates or edits it. Each line has exactly this versioned
shape (snake_case aliases for these field names are accepted):

```json
{"schemaVersion":1,"operationId":"recorded-operation-id","capturedAtMs":2000,"workContext":{"schemaVersion":1,"id":"context-one","repositoryId":"coordinator-repo","projectId":"project-one","workstreamId":"stream-one","outcomeId":"outcome-one"}}
```

The context's repository/project/workstream/outcome IDs are declared external
identities, not guessed translations of the producer's repository or thread ID.
Context IDs must have one consistent definition, and an operation may have only
one consistent binding. IDs allow ASCII letters, numbers, hyphens, underscores,
periods, and colons, with a maximum length of 256. All schemas must be version 1,
capture times must be nonnegative, and unknown fields or conflicting duplicate
bindings fail closed with a fixed error that omits private content and paths.
The source is bounded to 512 KiB, 256 nonempty lines, and 50 contexts. Writers
should publish complete files by atomic replacement.

The response joins declarations to actual scoped facts using only exact
`operation_id` equality. It returns those `operationBindings` and `workContexts`,
and groups measured effort under `review.outcomes.rows` by declared outcome ID.
An outcome row also carries its declared `workstreamId`. Contexts without an
outcome ID, missing or retired operation facts, response-only token links, and
operations without bindings stay unattributed. Binding declarations alone never
create operations, tokens, time, or delivery receipts. With no sidecar, the
binding arrays remain empty and all effort remains unattributed.

`report.workBindings` identifies the `explicit_sidecar` basis, bound/unknown
operation counts, and a readable `sourceRevision`. That revision includes line
count, latest capture time, a process revision counter, and session timestamp.
Content hashing is private; changed bytes receive a new revision even when line
counts and capture times are unchanged. The composite `snapshot.sourceWatermark`
includes that revision, so sidecar changes invalidate continuations too. The
native `summary` remains the underlying storage reader's view; composite binding
coverage belongs to `report.workBindings` and `review.outcomes`.

Outcome coverage is `unattributed`, `partial`, or `complete` according to the
measured facts that could be explicitly joined. These are accounting coverage
states, not proof of a delivered outcome. Missing measurements stay unknown.
The retained source fixture is
`codex-rs/app-server/tests/fixtures/local_usage/work-contexts.jsonl`; the RPC tests
cover both the absent-source case and a bound outcome alongside unbound effort.
`codex-rs/app-server/tests/fixtures/local_usage/bound-summary.json` retains an
actual app-server RPC response from the isolated test data: 120 measured tokens
are explicitly bound to `outcome-one`, while 15 remain unattributed. Its capture
and revision timestamps are test-run metadata, not production usage evidence.
