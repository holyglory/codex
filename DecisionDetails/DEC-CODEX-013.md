# DEC-CODEX-013: Descending account drain priority

## Confirmed behavior

The priority value is a drain rank, not a queue position. Automatic selection
evaluates eligible locally managed ChatGPT OAuth profiles from the largest
numeric priority to the smallest. A smaller value therefore drains later.

The highest eligible priority tier is authoritative at every new turn. The
current account is retained when it remains eligible and belongs to that tier;
otherwise selection moves to an eligible higher-ranked profile. Equal-ranked
profiles use a deterministic stable tie-break and retain the current profile
when possible so automatic selection does not oscillate.

New and legacy-migrated profiles default to priority `1000`. Existing stored
priorities are not silently rewritten during software upgrade; the authorized
`slawa` deployment is explicitly normalized to `1000` through one registry
mutation. Users and agents may later set individual or all priorities through
generation-aware metadata operations.

## Preserved boundaries

Priority never broadens automatic eligibility beyond DEC-CODEX-009. Unknown or
stale capacity still follows the bounded probe and fail-closed rules in
DEC-CODEX-011, and one immutable account lease remains fixed for each turn.
Priority changes affect later turn boundaries and never replace credentials in
an active turn.

## Verification

Selector and router tests cover a lower-ranked current account, descending
fallback across exhausted tiers, stable equal-rank retention, stale-limit
probe order, excluded authentication modes, and concurrent turns. CLI,
app-server, TUI, and agent-tool tests cover the `1000` default, list ordering,
generation conflicts, one-operation set-all behavior, and credential-free
output. Live verification confirms all `slawa` profiles are priority `1000`.
