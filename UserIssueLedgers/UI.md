# User Issue Ledger: UI

| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |
| --- | --- | --- | --- | --- |
| UIL-UI-001 | Usage requests in the ChatGPT desktop/Codex chat surface and the Codex TUI | Describing a TUI-only slash-command interception as universal causes `/usage all` to be submitted as an ordinary model request on chat surfaces and leaves the user waiting without the requested report | Identify the active client surface: the TUI may intercept `/usage ...` locally, while a model-routed chat must call the built-in `usage_stats` tool immediately and return the report without documentation research or internal implementation identifiers | Exercise `/usage all` through each supported rendered surface, assert that the TUI opens its local usage view and that model-routed chat invokes `usage_stats` directly, and verify a bounded truthful result or concise user-facing failure with no internal ledger IDs |
