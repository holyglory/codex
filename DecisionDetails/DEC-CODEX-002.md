# DEC-CODEX-002: Required local token and tool usage accounting

## Evidence and context

The user requires detailed statistics explaining token and tool usage for every
chat and repository, plus totals across repositories and operation categories
such as coding, testing, deployment, documentation, specification, and
user-facing elaboration. The user additionally confirmed that the agent can
regularly inform the stats engine of its current category through a short tool
call.

Current official OpenAI documentation establishes useful capture surfaces:

- App-server emits `thread/tokenUsage/updated` and typed lifecycle items for
  command, file-change, MCP, dynamic, collaboration, web-search, and image-view
  activity: <https://learn.chatgpt.com/docs/app-server>.
- Codex OTel events include response-completion token counts and structured
  tool result metadata, but OTel export is optional:
  <https://learn.chatgpt.com/docs/config-file/config-advanced>.

The architecture therefore treats provider-completion usage and core tool
lifecycle events as facts, while using app-server totals for reconciliation and
keeping optional OTel independent.

## Options considered

- OTel-only export was rejected as the authoritative store because it is
  optional, externally configured, and does not by itself provide the required
  local retention, repository model, corrections, or stable reports.
- Rollout/terminal/log scraping was rejected because presentation formats are
  not a stable event contract and can include prohibited content.
- A networked collector was rejected for the approved local single-owner scope
  because it adds deployment, authentication, availability, and privacy
  boundaries without being required.
- A private SQLite database per `CODEX_HOME` was selected because it is
  embedded, transactional, queryable across repositories, and compatible with
  concurrent local Codex processes when implemented with reviewed locking,
  migrations, and recovery.
- Heuristic-only semantic classification was rejected because it could present
  guesses as exact attribution. A short, enum-only `usage_activity` tool sets
  the category for the next substantive span. The tool call is
  `agent_declared`, its own token/tool cost is `accounting_overhead`, and
  deterministic runtime evidence remains separate. The full extra boundary
  response is measurable overhead; any marginal schema cost on unrelated
  requests remains unknown unless the provider reports it separately.

## Measurement rules

Provider-native token categories remain separate and nullable. One response's
usage is never proportionally divided across semantic activities without an
actual provider boundary. Mixed or unsupported attribution is reported as
`mixed` or `unknown`. Measurement provenance is separate from classification
provenance. Retries, rework, delegated agents, and concurrent spans remain
visible and additive only where mathematically valid.

Source inspection found that upstream v0.149.0 maps some absent usage details
to zero, drops bounded future categories, and does not surface all failed,
incomplete, or image-generation usage. Accounting therefore uses a parallel
presence-preserving provider-usage model while leaving the existing
compatibility `TokenUsage` unchanged.

## Data and compatibility consequences

Capture runs in the shared core path for CLI, TUI, exec, SDK, MCP, and
app-server callers. A durable start event precedes every model/tool operation;
capture failure prevents a new operation rather than creating a silent gap.
The database stores categorical metadata only and is independent of upstream
thread history, OTel, analytics, and `account/usage/read`.

Provider-hosted tools execute remotely before the client observes their item.
Their durable covering record is the parent model-request start, followed by an
observed hosted-tool event. Client-invoked tools retain independent durable
start gates.

The initial scope aggregates all repositories within one `CODEX_HOME`. It does
not combine separate Unix users' databases or expose a network service.

Upstream v0.149.0 already uses `/usage` and `account/usage/read` for
service-backed account activity. Local accounting therefore extends the
existing `/usage` menu and uses a distinct `localUsage*` app-server namespace;
it never reinterprets service totals as local chat measurements.

## Verification

Section 21 of `HANDOVER.md` defines the schema, taxonomy, capture semantics,
privacy boundary, reports, failure behavior, and acceptance criteria. Required
tests reconcile provider usage through chat/repository totals; exercise
retries, forks, delegated agents, concurrency, crashes, migrations, corruption,
and disk exhaustion; and prove prohibited content cannot enter the database,
exports, logs, snapshots, or RPC responses.
