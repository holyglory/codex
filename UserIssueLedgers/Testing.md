# User Issue Ledger: Testing

| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |
| --- | --- | --- | --- | --- |
| UIL-TESTING-001 | `/home/CodexMulti` test, build, schema, runtime, and handoff workflows | Continuing to invoke DevCoordinator for this repository after it duplicated resource-heavy runs and the user withdrew and later reaffirmed withdrawal of its use | Do not invoke DevCoordinator for this repository unless the user explicitly requests it for a concrete operation | Before orchestration, read this ledger and use repository-local commands only; report local environment limitations honestly and verify that no Coordinator plan, submission, follow, retry, cancellation, deployment, or runtime action is performed |
