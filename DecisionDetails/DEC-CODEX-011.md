# DEC-CODEX-011: Pre-turn rate-limit probes

## Confirmed behavior

Automatic selection explicitly permits a read-only rate-limit GET when an
eligible managed ChatGPT OAuth profile has missing or stale capacity evidence.
The process probes the current profile first, then candidates in deterministic
priority order, and re-runs the pure fail-closed selector after each successful
observation.

The probe runs before any model or tool request, under a temporary profile lease
that prevents credential removal during the request. One process-wide
singleflight coalesces concurrent first turns. No registry, cache, or selection
mutex remains held across network I/O.

## Failure and scope boundaries

- Only `AuthMode::Chatgpt` profiles are probed; DEC-CODEX-009 remains the
  billing and ownership boundary.
- Each service request is time-bounded. A timeout, authentication failure,
  invalid response, or other fetch error leaves that profile unknown.
- Unknown capacity is never reinterpreted as available. If no fresh eligible
  profile can be established, the existing content-free selection error is
  returned before model work.
- A process pin and disabled automatic selection bypass probing entirely.
- Quota snapshots remain process-local; a separate CLI process cannot warm or
  authorize another app-server process.

This decision closes the failure recorded in
`UIL-BUSINESS-LOGIC-ACCOUNT-SELECTION-001` and completion issue
`CML-6E81C4DDB12D` once the original SSH/UI turn succeeds after deployment.
