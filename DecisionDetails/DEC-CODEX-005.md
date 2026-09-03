# DEC-CODEX-005: Exact usage and hosted-tool capture boundary

## Evidence

At upstream v0.149.0, normal completed Responses usage is emitted from
`codex-rs/core/src/session/turn.rs`, but the transport retry boundary lives in
`codex-rs/core/src/client.rs`. The existing cumulative token contributor lacks
request attempt, account profile, model, and repository identity.

The Responses parser converts some missing cached/reasoning details to zero and
drops unknown numeric categories. Failed/incomplete metadata and image endpoint
usage are not consistently surfaced. Existing tool lifecycle contributors run
after pre-hooks and cannot cover every rejected or unsupported invocation.

Provider-hosted tools execute at the provider; the local client learns about
them only after execution appears in a response item.

## Decision

- Preserve the existing fixed `TokenUsage` contract for upstream consumers.
- Add a parallel bounded provider-usage observation that preserves presence,
  future numeric keys, and exact usage returned by supported endpoints.
- Instrument HTTP and WebSocket attempt boundaries directly with durable local
  IDs and safe metadata; use cumulative notifications only for reconciliation.
- Gate every client-invoked tool around the complete registry dispatch,
  including pre-hook rejection and unsupported/incompatible calls.
- Treat a durable model-request attempt as the covering start for remotely
  hosted tools. Append an `observed_after_execution` tool row when received.
- Record token coverage as unknown whenever a provider does not return exact
  usage. Do not infer or tokenize content to manufacture a count.

## Consequences

The stats engine distinguishes provider-reported, runtime-observed, covered,
and unknown data. Reports cannot claim literal pre-execution capture for hosted
tools or exact nested usage for search, realtime, memory-summary, arbitrary MCP,
or third-party models that do not return it.

## Verification

Fixtures cover absent versus zero fields, unknown numeric categories,
completed/failed/incomplete responses, retries, image usage, client-tool
pre-hook rejection, hosted-tool coverage linkage, and nested providers with
unknown token coverage.
