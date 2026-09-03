# DEC-CODEX-012: Subscription capacity before top-up credits

## Confirmed interpretation

`CreditsSnapshot.has_credits` describes optional purchased/top-up credit
availability; it is not by itself proof that a ChatGPT subscription cannot run
a Codex turn. Live Pro accounts can report `has_credits=false` while their
primary subscription window remains well below 100% usage.

Automatic eligibility evaluates evidence in this order:

1. An explicit backend `rate_limit_reached_type` is authoritative.
2. Reached spend control, a full subscription window, or a zero individual
   allowance is reached.
3. A valid open subscription window, positive credits, or unlimited credits is
   eligible.
4. Depleted credits are a reached fallback only when no usable window or
   positive-credit evidence exists.
5. Invalid, expired, or contradictory window evidence remains unknown and
   fail-closed.

This preserves explicit service denials while preventing the false rejection
recorded in `UIL-BUSINESS-LOGIC-ACCOUNT-SELECTION-002` and completion issue
`CML-2E82B15F6674`.
