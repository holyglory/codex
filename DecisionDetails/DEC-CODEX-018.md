# DEC-CODEX-018: Align Responses retries with upstream

The user reported repeated Desktop capacity errors on vr.ae and identified the
fork's Responses retry implementation as excessive divergence (2026-09-17).

Restore the behavior from official `rust-v0.154.0` and the canonical checkout's
pre-policy implementation. Reverse the capacity policy introduced by
`05e0b78bbba6ca6e3ee1e92e7daceefa116dff89`, including its cross-layer retry
deferral, dedicated jittered budget, and modified regression expectations.
Keep the independent WebSocket handshake fallback change and all account and
usage work. This is a targeted cleanup, not a wholesale upstream upgrade.

The policy's nominal delay sequence is 0, 5, 30, 120, and 300 seconds with
1–2× jitter, totaling about 455–910 seconds before exhaustion. A second policy
across three retry layers increases maintenance without explaining the observed
failure: the live daemon's executable maps to `2300d76ae7b0`, whose
`responses_retry.rs` matches official `rust-v0.154.0` byte for byte.

Initial verification restored existing upstream tests rather than asserting a
new downstream retry policy: 189 transport/API tests and 30 focused core checks
pass. The authorized complete workspace run reported 17,332 passed, 59 failed,
and 31 skipped. All 59 failures reproduced when the cleanup was reversed in
the isolated checkout, establishing that they are present in the original
dirty source/environment. The candidate was restored and its 11 file hashes
verified afterward. These results do not qualify the wider dirty checkout for
release; unrelated baseline repairs remain outside this cleanup.

Cold test evidence lives on vr.ae under
`/home/CodexMulti/.state/capacity-diagnosis-20260917-input/` in
`focused-producers.log`, `focused-core-with-host.log`, `full-workspace.log`, and
`baseline-failures.log`.

Scoped `just fix -p codex-client -p codex-api -p codex-core` passed without
warnings or source changes. The reviewed cleanup was applied to the canonical
dirty checkout after checking its original diff hash. All 110 originally dirty
or untracked files were verified against the intended candidate, including the
11 cleanup files. The running daemon and installed binaries were unchanged.

Fresh no-tools requests succeeded through the live daemon, including an
ephemeral fork preserving the failing task's instructions and configuration.
Recorded metadata confirms the fork used the same account, model, and WebSocket
transport as the original failures. A separate process pinned to the user's
reference account also succeeded on vr.ae. These probes establish availability
at the time of testing, not recovery of the original failed Desktop task.

The user subsequently confirmed normal operation on Desktop. A follow-up check
verified that VPS daemon PID 2981107 was still running the same executable.
The incident is therefore recorded as recovered with user confirmation, without
attributing recovery to the undeployed source cleanup. The exact backend or
session cause remains unknown; the resolved incident item was removed from
`CompletionLedger.md`, which had no other open items.
