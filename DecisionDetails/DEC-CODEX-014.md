# DEC-CODEX-014: Complete operator and agent access to local accounting

## Confirmed outcome

The operator can inspect every approved content-free collector dimension for a
chat, repository, account, or installation through stable CLI and app-server
data, with a full-fidelity TUI view for interactive use. In-product agents can
request the same summary dimensions and page through safe repository, tool,
activity, event, entity, operation/model attempt, token, approval, attribution,
classification, coverage, wait, lifecycle, repository-evidence, and taxonomy
records through a bounded built-in tool.

Agents can also list nonsecret account-routing state and change account
priorities through a separate bounded local tool. The mutation surface is
generation-aware and does not authorize credential reads, login completion,
credential deletion, or opaque service-identity access. Human CLI operations
remain available for the full account lifecycle.

Historical usage edits are limited to append-only categorical classification
corrections through `usage_activity`; they accept only a safe target identifier
and existing phase/activity enums. No agent tool can delete accounting facts,
merge repositories, or create an export.

## Data and context boundaries

These interfaces read the existing per-`CODEX_HOME` database and registry;
they do not create a shared database, listener, exporter, or cross-user
aggregation service. Outputs preserve provider-native token categories,
coverage, measurement and classification provenance, timing unions, agent
concurrency, participation counts, tool outcomes, formulas, and explicit
unknowns. Detailed rows are paginated and each model-visible response has a
hard size bound.

There are no older deployed multi-account/local-usage servers to support.
Enhanced clients and servers therefore move together to capability version 2
with the complete fields required; stock upstream clients remain compatible by
ignoring the optional extension capabilities and methods.

Credentials, authorization material, email addresses, opaque service or
workspace identifiers, prompts, model output, source, commands, tool payloads,
raw paths/remotes, and raw errors remain excluded as required by
`security-assumptions.md` and DEC-CODEX-002. Repository aliases and local
profile aliases are resolved at query time.

## Repository identity

Current-repository lookup evaluates every verified identity available for the
checkout in authority order: normalized remote, Git common directory, then
canonical workspace root. It matches any previously captured identity and
resolves append-only merge history without persisting raw identity material.

## Verification

Focused tests round-trip every summary field through CLI, app-server/TUI, and
the built-in agent tool; exercise pagination and output bounds; resolve current
repositories across richer and poorer Git metadata; and prove prohibited
content and credentials remain absent. Live `slawa` verification covers the
current `/home/CodexMulti` repository, all-repository totals, account-filtered
usage, account list/priority mutation, and rollback-safe deployment.
