# User Issue Ledger: Automation

| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |
| --- | --- | --- | --- | --- |
| UIL-AUTOMATION-001 | Long-running repository builds and release preparation interrupted by informational questions | Answering a status or installation question and leaving the task idle silently abandons an unfinished authorized release | Preserve the agreed outcome across informational questions; answer briefly, reconcile retained operations and the authoritative ledger, then continue dependency-ready work until the outcome is verified or a concrete external gate requires the user | Before ending a turn, reconcile the original acceptance criteria, every retained operation, and request-related unfinished outcomes; verify that an informational question did not replace the objective and that any actual stop names its blocker and smallest required user action |
