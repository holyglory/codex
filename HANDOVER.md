# Codex Multi-Account CLI Fork: Engineering Handover

Status: `0.153.0-alpha.6+multi.4` rebased, source-validated, packaged, and deployed only for `holyglory`; `slawa` remains on verified `0.149.1+multi.1`; `holygloryTT` and `axel` remain unchanged

Last updated: 2026-09-03

Deployment host: `vr.ae`

Authorized deployment accounts: `slawa` and `holyglory`, one explicit target per operation

Intended repository location on the server: `/home/CodexMulti`

Pinned upstream baseline: `rust-v0.153.0-alpha.6`
(`e8b3253fed5aeef7e914441bc3b73b3b0a718b51`)

## 1. Purpose

Build and maintain a downstream version of the open-source OpenAI Codex CLI
that behaves like the official Codex CLI while adding first-class support for
multiple authenticated accounts.

The delivered executable must remain named `codex`. Existing programs,
scripts, users, MCP clients, and remotely connected Codex applications should
continue to use it through the same command-line and app-server interfaces.

The added functionality must support:

1. Listing configured accounts.
2. Viewing nonsecret account information and current usage limits.
3. Authorizing a new account through supported Codex login flows.
4. Renaming and editing account metadata.
5. Selecting the active account manually.
6. Selecting an account for only one process or invocation.
7. Disabling and removing accounts.
8. Automatically selecting another eligible account before a turn when the
   current account has reached a server-reported limit.
9. Allowing external scripts to perform the same account operations through
   stable CLI commands and additive app-server RPC methods.
10. Recording durable, detailed local accounting for model-token usage and
    tool activity for every new chat, turn, agent, and repository handled by
    the customized build.
11. Classifying that usage by the operation being performed, including
    requirements, specification, analysis, coding, test authoring, testing,
    deployment, documentation, and user-facing explanation, while preserving
    the distinction between measured, declared, inferred, and unknown data.
12. Reporting both detailed per-chat usage and deduplicated totals across all
    repositories known to one `CODEX_HOME` through stable CLI, TUI, JSON, and
    additive app-server interfaces.

## 2. Authoritative Upstream

The authoritative upstream project is:

- Repository: `https://github.com/openai/codex`
- Primary implementation: the Rust workspace under `codex-rs/`
- Official integration boundary: `codex app-server`

OpenAI documents Codex CLI and Codex app-server as open-source components. The
license, notices, and redistribution requirements in the exact upstream
revision being built must be preserved. Do not assume that source-code license
terms change OpenAI service authentication, subscription, workspace, usage, or
rate-limit terms.

## 3. Repository Strategy

### 3.1 Required model

Maintain a direct fork rather than editing an untouched upstream submodule:

```text
origin   -> the private or organization-owned Codex fork
upstream -> https://github.com/openai/codex.git
```

Recommended long-lived branches:

```text
upstream-sync    exact or minimally transformed upstream state
multi-account    maintained product branch containing our changes
```

Release branches and tags should identify both versions:

```text
release/0.148.0-multi.1
v0.148.0-multi.1
```

An optional outer distribution repository may include this fork as a
submodule for packaging, installers, and deployment manifests. The code fork
itself must remain independently buildable and testable.

### 3.2 Patch organization

Keep downstream commits narrow and grouped by subsystem so upstream merges are
reviewable:

1. Account registry and metadata model.
2. Profile-aware credential storage.
3. Authentication-manager account switching.
4. CLI account commands.
5. TUI account commands and selector.
6. Additive app-server protocol methods.
7. Rate-limit-aware selection and failover.
8. Migration and compatibility behavior.
9. Local usage-accounting storage and capture.
10. Usage classification and reporting interfaces.
11. Tests and release automation.

Avoid unrelated formatting, renaming, dependency upgrades, or refactors in
downstream commits.

## 4. Compatibility Contract

### 4.1 Existing executable behavior

The customized binary must preserve all existing upstream behavior unless an
upstream change intentionally changes it:

- Executable name: `codex`
- Existing subcommands and flags
- Exit codes
- Human-readable output relied upon by users
- Machine-readable and JSONL output relied upon by scripts
- `codex exec`
- `codex login`
- `codex logout`
- `codex app-server`
- `codex mcp-server`
- Configuration loading and profiles
- `AGENTS.md`, skills, plugins, and MCP configuration
- Thread creation, persistence, resume, fork, archive, and history
- Sandbox and approval behavior
- Model selection and collaboration modes

New behavior must be additive. Existing command names and flag meanings must
not be repurposed.

The local usage-accounting database must not replace, mutate, or reinterpret
upstream thread history, upstream OTel export, or the service-backed
`account/usage/read` response. Those remain separate compatibility surfaces.
The fork's database is a local, detailed operational record with explicit
coverage and attribution provenance.

### 4.2 Existing app-server behavior

Every upstream app-server method, notification, error shape, and initialization
contract must remain compatible with the corresponding upstream release.

Existing singular account methods continue to operate on the active account:

```text
account/read
account/login/start
account/login/cancel
account/logout
account/rateLimits/read
account/rateLimits/updated
account/usage/read
account/updated
```

An unmodified client must be able to use the fork without understanding the
multi-account extension. It sees one active account exactly as it does with
official Codex.

### 4.3 Remote Codex application

A remotely connected, unmodified Codex application must continue to:

1. Start or connect to app-server.
2. Initialize successfully.
3. Read the active account.
4. List models.
5. Create and resume threads.
6. Start, steer, interrupt, and complete turns.
7. Handle approvals and user-input requests.
8. Receive normal streamed events.
9. Receive `account/updated` after an account switch.

The unmodified application will not automatically gain a graphical account
manager. Account management remains available through the customized CLI,
TUI, external scripts, and additive RPC methods. A future enhanced client can
use the extension methods after capability detection.

### 4.4 Version reporting

Report the upstream-compatible base version and downstream build identity
truthfully. Prefer SemVer build metadata where accepted:

```text
codex-cli 0.148.0+multi.1
```

If any client rejects build metadata, preserve the upstream protocol version
and expose the downstream revision through a separate diagnostic command or
optional initialization metadata. Do not silently claim that a modified build
is an unmodified official binary.

## 5. Account Model

### 5.1 Stable identity

Each stored account has:

- A generated stable profile ID, independent of email and display name.
- A unique user-facing alias.
- Authentication mode.
- Nonsecret account metadata returned by Codex services.
- Enabled or disabled state.
- Selection priority.
- Creation and last-used timestamps.
- Optional user note.

Do not use email as a primary key. Emails and workspace memberships can change.
When available, retain the service account/workspace identifier as protected
metadata for duplicate detection, but never print opaque identifiers in normal
UI output.

### 5.2 Suggested metadata schema

```json
{
  "version": 1,
  "generation": 12,
  "defaultAccountId": "01J00000000000000000000001",
  "autoSelection": {
    "enabled": false,
    "policy": "priority"
  },
  "accounts": [
    {
      "id": "01J00000000000000000000001",
      "alias": "holyglory",
      "authMode": "chatgpt",
      "email": "redacted@example.invalid",
      "planType": "unknown",
      "enabled": true,
      "priority": 1000,
      "createdAt": "2026-08-20T00:00:00Z",
      "lastUsedAt": null,
      "note": null
    }
  ]
}
```

The example is synthetic. Tests and documentation must not contain real
credentials or production tokens.

## 6. Credential Storage

### 6.1 File-backed layout

Recommended layout under `CODEX_HOME`:

```text
~/.codex/
|-- config.toml
|-- accounts/
|   |-- index.json
|   |-- 01J00000000000000000000001/
|   |   `-- auth.json
|   `-- 01J00000000000000000000002/
|       `-- auth.json
|-- active-account
|-- sessions/
|-- skills/
`-- AGENTS.md
```

Requirements:

- The account directory must not be world-readable.
- Credential files must have permissions equivalent to upstream `auth.json`.
- Writes must use an atomic replace in the same filesystem.
- The parent directory must be synchronized when durability matters.
- Partial writes must not destroy the last valid credential record.
- Credential JSON must never be written to logs or error telemetry.

### 6.2 Keyring-backed layout

When the upstream credential-store mode is `keyring` or resolves to keyring,
namespace each credential by stable profile ID. Metadata may remain in the
protected account index, but access and refresh tokens stay in the operating
system credential store.

The keyring service/account naming convention must be versioned and covered by
migration tests.

### 6.3 Storage abstraction

Refactor the existing single-account storage through a profile-aware layer.
Suggested concepts:

```rust
pub struct AccountId(String);

pub struct AccountRegistry {
    // Nonsecret metadata, active/default selection, and generation.
}

pub struct ProfileAuthStorage {
    account_id: AccountId,
    backend: Arc<dyn AuthStorageBackend>,
}

pub struct ActiveAccountSnapshot {
    account_id: AccountId,
    generation: u64,
    auth: Arc<CodexAuth>,
}
```

Reuse the upstream `AuthStorageBackend` behavior where possible. Do not fork
OAuth parsing, token refresh, or request authentication into unrelated code.

## 7. Migration From Official Codex

On first launch of the customized build:

1. Detect whether the multi-account registry already exists.
2. If it exists, do not import again.
3. If it does not exist and legacy `CODEX_HOME/auth.json` exists, lock the
   migration target.
4. Validate the legacy credential file using the upstream parser.
5. Create an account with alias `default`, or a sanitized email-derived alias
   when available and unambiguous.
6. Atomically write the account registry and profile credential record.
7. Verify that the new profile can be loaded.
8. Preserve a recoverable legacy copy until the new record is verified.
9. Complete or roll back the migration as one operation.
10. Never log token data during migration.

After successful migration, normal operation must not depend on a mirrored
legacy `auth.json`; dual writable copies would create refresh races and stale
credentials.

The official CLI should remain recoverable by an explicit export operation if
needed, rather than through continuous mirroring.

## 8. CLI Interface

### 8.1 Commands

Implement at least:

```text
codex account list [--json]
codex account current [--json]
codex account show <alias-or-id> [--json]
codex account add <alias> [--device-auth]
codex account rename <account> <new-alias>
codex account edit <account> [--priority N] [--note TEXT]
codex account priority [list]
codex account priority set <account> <N>
codex account priority set-all <N>
codex account use <account>
codex account enable <account>
codex account disable <account>
codex account remove <account>
codex account limits [<account>] [--all] [--json]
codex account auto [status|on|off]
```

Implement local usage reporting commands without changing the meaning of the
service-backed account usage interfaces:

```text
codex usage summary [--since TIME] [--until TIME] [--json]
codex usage chat <thread-or-current> [--breakdown DIMENSION] [--json]
codex usage repo [<repo-or-current>] [--breakdown DIMENSION] [--json]
codex usage repositories [--cursor CURSOR] [--limit N] [--json]
codex usage tools [--thread ID] [--repo REPO] [--json]
codex usage activities [--thread ID] [--repo REPO] [--json]
codex usage events [--thread ID] [--repo REPO] [--json]
codex usage details <record-family> [--cursor CURSOR] [--limit N] [--json]
codex usage classify <operation> --phase PHASE --activity ACTIVITY
codex usage repo alias <repo> <alias>
codex usage repo merge <source> <target>
codex usage export [--thread ID] [--repo REPO] --format jsonl|csv
codex usage doctor [--json]
```

Shared filters must include account, model, root or delegated agent, phase,
activity, tool, repository, thread, turn, status, provenance, coverage, and
time range. `--breakdown` must accept multiple dimensions in stable JSON mode.
Human-readable reports should lead with the selected chat or repository and
show incomplete or unknown coverage before totals.

`current` is valid only where the running client has an actual active thread,
such as the TUI. A standalone CLI process must receive a thread ID for a chat
report; it must not guess from the most recently modified thread. Repository
`current` resolves only the verified current workspace.

Also support a process-local override:

```text
codex --account <alias-or-id>
codex exec --account <alias-or-id> "request"
codex app-server --account <alias-or-id>
```

### 8.2 Compatibility of login and logout

Preserve existing commands:

- With no account registry, `codex login` creates or authorizes the default
  account.
- With an active account, `codex login` replaces or repairs that active
  account's credentials, matching the previous singular behavior.
- `codex logout` logs out the active account but does not delete unrelated
  profiles.
- `codex account add` is the explicit path for authorizing another profile.
- `codex account remove` deletes metadata and credentials after confirmation.

### 8.3 Machine-readable output

Every account command needed by automation must support stable JSON output.
Version the schema and exclude secrets:

```json
{
  "schemaVersion": 1,
  "activeAccount": "axel",
  "accounts": []
}
```

Errors must have deterministic nonzero exit codes. Distinguish at least:

- Unknown account
- Ambiguous account
- Disabled account
- Not authenticated
- Login cancelled
- Credential-store failure
- Account in use by an in-flight operation
- No eligible account
- Rate-limit information unavailable

Usage-report JSON must also be versioned. Counts must retain provider-native
token categories and nullable or unknown values; JSON output must not fabricate
a single exact total when the provider did not report enough information.

## 9. TUI Interface

Add slash commands without changing existing commands:

```text
/account
/account list
/account use <account>
/account add <alias>
/account edit <account>
/account remove <account>
/account limits
/account auto
/usage
/usage chat
/usage repo
/usage tools
/usage daily
/usage weekly
/usage cumulative
```

`/account` should open a keyboard-accessible selector showing:

- Active marker
- Alias
- Email when available
- Plan type when available
- Enabled state
- Primary and secondary limit state
- Reset time when returned by the service
- Current selection policy

Never display:

- Access tokens
- Refresh tokens
- Raw authorization headers
- Complete opaque account or workspace IDs
- Serialized `auth.json`

Changing the account while a turn is running must either be deferred until the
turn boundary or clearly rejected. It must never replace credentials under an
in-flight request.

Upstream v0.149.0 already uses `/usage` for a menu of service-backed account
usage and rate-limit-reset actions, with `/usage daily`, `/usage weekly`, and
`/usage cumulative` inline forms. Preserve those commands and results exactly.
Extend the existing bare `/usage` menu instead of repurposing it: when local
accounting is available, "Current chat statistics" is the first item, followed
by the existing account-usage/reset actions. `/usage chat`, `/usage repo`, and
`/usage tools` open the corresponding local views. On an older remote server
without the local-usage capability, preserve the original menu and auth gating.

The local current-chat view must show coverage, provider-reported token
categories, operation classification, tool counts and durations, and root or
delegated-agent attribution. Repository and all-repository views are secondary
drill-downs. No usage view may expose prompt content, model output, reasoning
content, commands, tool arguments/results, source paths, or secret or opaque
service/account identifiers.

## 10. App-Server Extension

### 10.1 Additive methods

Add capability-gated methods using the current app-server singular-resource
convention. Finalized v1 surface:

```text
accountProfile/list
accountProfile/read
accountProfile/activate
accountProfile/update
accountProfile/remove
accountProfileLogin/start
accountProfileLogin/cancel
accountProfileRateLimit/read
accountAutoSelection/read
accountAutoSelection/write
```

Add a separate capability-gated local usage namespace. Finalized v1 surface:

```text
localUsage/summary
localUsageThread/read
localUsageRepository/list
localUsageRepository/read
localUsageTool/list
localUsageActivity/list
localUsageEvent/list
localUsageClassification/correct
localUsageRepository/update
localUsageRepository/merge
localUsageExport/create
```

Add optional `localUsage/updated` and `accountProfile/activeChanged`
notifications for enhanced clients. Generate all request, response,
notification, JSON Schema, and TypeScript definitions through the upstream
schema workflow. Stock clients must be able to ignore the capabilities and
notifications.

Names must be finalized against current upstream protocol conventions before
implementation. Add new request, response, notification, JSON Schema, and
TypeScript definitions through the existing protocol-generation workflow.

### 10.2 Capability advertisement

Advertise the extension as optional initialization metadata without modifying
required upstream fields:

```json
{
  "multiAccount": {
    "version": 1,
    "supportsManagedLogin": true,
    "supportsAutoSelection": true
  }
}
```

An upstream client that does not inspect this field must continue normally.

Advertise local usage accounting through a separate optional capability so a
client can detect it independently of multi-account support:

```json
{
  "localUsageAccounting": {
    "version": 1,
    "requiredCapture": true,
    "supportsDetailedBreakdown": true
  }
}
```

### 10.3 Existing notifications

After activation changes, emit the existing `account/updated` notification
with the newly active account's normal upstream-compatible auth mode and plan
information.

Add an optional extension notification for enhanced clients:

```text
accountProfile/activeChanged
```

Do not add secret-bearing fields to either notification.

## 11. Authorization Flows

Reuse official Codex implementations for:

- Browser-based ChatGPT OAuth
- Device-code authentication
- API-key authentication when supported
- Access-token authentication when supported
- Token refresh
- Login cancellation
- Login restrictions and workspace restrictions

`account add` or `account/profiles/login/start` creates a pending profile that
does not become active until authorization succeeds, unless the caller
explicitly requests activation.

On successful login:

1. Validate returned authentication state.
2. Detect an already registered service account when possible.
3. Prevent accidental duplicate registration or require explicit confirmation.
4. Store credentials in the pending profile's backend.
5. Store only nonsecret metadata in the registry.
6. Emit login completion.
7. Activate only according to the request.

On cancellation or failure, remove incomplete profile state and preserve the
previous active account.

## 12. Selection Scope and Concurrency

Support two selection scopes:

### 12.1 Global default

`codex account use <account>` updates the default active account in the shared
registry. New processes use this account by default.

Long-running customized processes should observe registry generation changes.
They may apply a global change at the next safe turn boundary and emit
`account/updated`.

### 12.2 Process-local pin

`--account <account>` pins one process. Global account changes do not alter the
pinned process. This is required so concurrent TUI, app-server, and automation
sessions do not unexpectedly take control of each other.

Environment/API host overrides must not silently defeat an explicit pin. If
`--account` is combined with `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, externally
managed ChatGPT auth, workload identity, Agent Identity selection, Bedrock host
auth, or another externally owned credential source, reject the conflicting
configuration before a model request and name the nonsecret source category.
The caller must explicitly remove the override or omit `--account`. An API-key,
PAT, Agent Identity, or Bedrock credential deliberately stored in the selected
local profile remains valid; the conflict is about externally owned process or
host overrides, not the profile's own auth mode.

### 12.3 Locking

Implement:

- A registry lock for metadata and default-selection mutation.
- A separate per-account lock for credential writes and refresh.
- Atomic generation increments.
- Compare-and-swap or equivalent stale-write detection.
- Recovery from an abandoned lock according to a documented lock mechanism.

Never hold a filesystem lock across browser authorization or a model request.

## 13. Turn-Boundary Semantics

Every turn captures an immutable account snapshot before the first model
request:

```text
thread -> turn -> account snapshot -> authenticated model requests
```

Rules:

1. All requests belonging to one active turn use the same account unless the
   upstream request layer performs a safe refresh of that same account.
2. Manual switching during a turn takes effect at the next turn boundary.
3. A process-local pin remains effective until process exit or explicit change.
4. Thread history remains independent of credential storage.
5. Persist account alias or stable ID as diagnostic provenance only when doing
   so does not leak sensitive identifiers into user-visible history.
6. Start every turn and newly delegated agent with
   `unattributed/unknown`; do not inherit a stale activity category from a
   previous turn or parent agent.
7. Snapshot the selected stable local account reference into the usage turn,
   while resolving mutable aliases only when a report is queried.

## 14. Automatic Account Selection

### 14.1 Policy

The first supported policy is deterministic descending drain priority:

1. Inspect eligible accounts from the highest numeric priority to the lowest;
   smaller values drain last.
2. Retain the current account only when it remains eligible in the highest
   eligible priority tier. Equal-ranked accounts use a stable tie-break and
   retain the current account when possible.
3. Select the first authenticated account in the highest eligible tier. New
   profiles default to priority `1000`.
4. If rate-limit state is unknown, do not assume capacity. The policy permits a
   bounded read-only availability probe before model work, then requires the
   selector to re-evaluate the resulting evidence.
5. If no account is eligible, return a clear no-eligible-account result.

Do not implement random selection or concurrent duplicate execution.

The confirmed billing policy limits automatic candidates to locally managed
ChatGPT OAuth profiles (`AuthMode::Chatgpt`). Personal access tokens, API keys,
Bedrock credentials, Agent Identity, header-backed auth, and externally managed
ChatGPT tokens remain manually selectable or pinnable but are never entered
automatically. A manually activated excluded profile requires automatic
selection to be off; a process-local `--account` pin remains authoritative.
Do not silently broaden the automatic pool (DEC-CODEX-009).

When a managed ChatGPT profile's process-local limit snapshot is missing or
older than the freshness window, probe the current profile first and then
unknown candidates in priority order. Hold a temporary profile lease through
each GET, coalesce concurrent refreshes, and never hold registry/cache locks or
start model/tool work while probing. Failed probes remain unknown; they never
grant eligibility (DEC-CODEX-011).

Treat explicit backend reached types, spend controls, and subscription windows
as authoritative. A profile with an open subscription window remains eligible
when optional purchased-credit fields report no balance. Depleted top-up
credits are a reached fallback only when no usable subscription or positive
credit evidence exists (DEC-CODEX-012).

### 14.2 Retry safety

Automatic switching may occur before a new turn's first model request. If a
limit is reported after execution has begun:

- Retry transparently only when the failed request is known to be safe and no
  tool or external side effect can be duplicated.
- Never replay a whole turn that may already have modified files, launched
  commands, sent requests, or consumed approvals.
- Switch for the next continuation or ask for confirmation when replay safety
  is unknown.
- Preserve the original failure classification for diagnostics.

### 14.3 Service boundaries

Open-source modification of the client does not alter server-side account
entitlements or limits. Automatic selection must operate only across accounts
the operator is authorized to use. Do not describe it as bypassing service
limits.

### 14.4 Agent account management

Expose one bounded local `account_management` tool to Codex agents. Its list
action returns aliases, higher-first priorities, enabled/authenticated/default
state, exact current-turn routing by alias, and optionally fresh bounded
managed-ChatGPT service-limit usage. Mutations are limited to setting one or
all priorities with an optional registry-generation precondition. The tool has
a 16 KiB output cap and never returns email, credentials, notes, opaque service
or workspace identifiers, or destructive credential-lifecycle operations.
Login, authorization, rename, activation, and removal remain human CLI/TUI or
capability-gated app-server operations.

## 15. Expected Upstream Code Areas

Confirm current paths after cloning because upstream may move code. Areas known
to be relevant include:

```text
codex-rs/login/src/auth/manager.rs
codex-rs/login/src/auth/storage.rs
codex-rs/login/src/lib.rs
codex-rs/core/
codex-rs/otel/
codex-rs/protocol/src/auth.rs
codex-rs/app-server-protocol/src/protocol/v2/account.rs
codex-rs/app-server/
codex-rs/cli/
codex-rs/exec/
codex-rs/tui/
```

The implementation should extend upstream abstractions rather than duplicate
OAuth, HTTP authentication, rate-limit parsing, or app-server dispatch logic.

## 16. Testing Requirements

### 16.1 Unit tests

Cover:

- Registry creation and parsing
- Alias validation and uniqueness
- Stable ID lookup
- Atomic metadata updates
- File and keyring namespace resolution
- Per-account credential load/save/delete
- Legacy migration
- Duplicate-account detection
- Manual activation
- Process-local pinning
- Priority selection
- Disabled and logged-out accounts
- Unknown and stale rate-limit state
- Safe and unsafe retry classification
- Redaction of credential material

### 16.2 Concurrency tests

Cover multiple processes or realistic storage actors:

- Concurrent token refresh for the same account
- Refresh while another account becomes the default
- Two account metadata edits
- Account removal while selected by another process
- Abandoned or interrupted write
- Registry generation conflict
- App-server observing a default-account change
- Process-local pin resisting a global switch

### 16.3 CLI integration tests

Verify help text, exit codes, and JSON output for every new command. Verify that
all existing upstream commands continue to parse unchanged.

### 16.4 TUI tests

Verify selector rendering, keyboard navigation, cancellation, authorization
success and failure, account switching, disabled accounts, long aliases, and
terminal resize behavior. No secret may appear in snapshots.

### 16.5 App-server contract tests

For every supported upstream release:

1. Generate the official TypeScript and JSON schemas.
2. Generate schemas from the fork.
3. Confirm all official methods and required shapes remain compatible.
4. Confirm extension methods are additive and capability-gated.
5. Run initialize, account, model, thread, turn, approval, interrupt, and resume
   flows against the customized server.
6. Confirm stock clients can ignore extension notifications and metadata.

### 16.6 Real remote-application smoke test

Using a trusted test environment:

1. Connect the current Codex App to the customized app-server.
2. Start and complete a normal task.
3. Exercise an approval.
4. Resume the task after reconnecting.
5. Switch the default account externally.
6. Confirm the App receives normal account state and can start the next turn.
7. Confirm existing task history remains available.

### 16.7 Credential tests

Use fake tokens and fake OAuth responses in automated tests. Live credentials
must never enter source control, CI logs, snapshots, fixtures, or artifacts.

### 16.8 Usage-accounting tests

Cover:

- Exact preservation of every provider-reported token category
- Cumulative usage-event delta calculation without double counting
- Retries, reroutes, compaction, cancellation, interruption, and failed requests
- Root and delegated agents, including concurrent agents
- Agent-declared activity boundaries and accounting-overhead attribution
- Deterministic tool classification and conflicting declarations
- Mixed and unknown activity without invented token splits
- Built-in, shell, MCP, plugin/app, dynamic, browser, image, and collaboration tools
- Tool approval, denial, cancellation, timeout, failure, and recovery
- Git repositories, non-Git workspaces, worktrees, moved checkouts, and multi-repo chats
- Resumed, forked, archived, and deleted-account histories
- Concurrent writers, crash recovery, schema migration, corruption, and disk-full behavior
- Stable filters, pagination, JSON schema, JSONL/CSV export, and aggregation totals
- Deduplicated wall time versus summed per-agent active time
- Coverage gaps and incomplete-operation recovery
- Database, CLI, TUI, and app-server redaction of prohibited content
- Stock-client compatibility when all local-usage extensions are ignored

Tests must include realistic must-catch fixtures for leaked prompts, reasoning,
model output, source diffs, full paths, remote URLs, shell commands, tool
arguments/results, credentials, and raw error messages, plus false-positive
guards for permitted categorical metadata.

## 17. Upstream Release Workflow

For every official Codex release:

1. Fetch upstream tags and commits.
2. Record the selected upstream tag and commit.
3. Create an update branch.
4. Merge or rebase according to the repository's recorded policy.
5. Resolve conflicts without discarding upstream behavioral changes.
6. Regenerate protocol schemas and compare them with upstream.
7. Run the complete upstream test suite.
8. Run all multi-account and usage-accounting unit, integration, concurrency,
   migration, redaction, aggregation, and compatibility tests.
9. Build release binaries for supported platforms.
10. Run the real remote-application smoke test against a staging install.
11. Review dependency, license, and notice changes.
12. Publish only after all required gates pass.

Automated dependency or tag detection may open an update request, but must not
silently deploy a build whose auth or protocol compatibility has not passed.

## 18. Packaging and Installation

The initial deployment targeted only `slawa` (DEC-CODEX-010). The current
authorization additionally permits `holyglory`, while preserving the verified
`slawa` installation. Every delivery operation names exactly one of those two
accounts. Do not modify the launchers, current-release links, runtime data, or
processes of `holygloryTT` or `axel`.

The VPS installation must place the customized binary where all intended
launchers resolve it as `codex`. Before replacement:

1. Identify every installed official binary and symlink.
2. Identify how each user's shell resolves `codex`.
3. Identify how each remote app-server service resolves the binary.
4. Preserve a verified rollback binary.
5. Stop or drain active app-server processes without abandoning running turns.
6. Install the customized release atomically.
7. Restart only the selected account's app-server processes when applicable.
8. Verify version, account state, MCP startup, thread access, and remote
   connectivity under the selected account.

Do not overwrite user credentials during binary installation. Credential
migration belongs to the customized CLI and must be tested independently.

Public downstream distribution is separately authorized by DEC-CODEX-016. The
source fork is `https://github.com/holyglory/codex`, and the intended public npm
package is `@holyglory/codex`. The package has not yet been submitted or
published. Build and verify all six native platform payloads before the root
wrapper; bootstrap the first package through the operator's interactive npm
session, then configure `downstream-npm-release.yml` as the sole stage-only OIDC
trusted publisher. npm publication never authorizes a VPS deployment or the
inclusion of credentials and per-user runtime data in an artifact.
The Rust version `0.153.0-alpha.6+multi.4` maps to the npm-safe version
`0.153.0-alpha.6-multi.4` because npm strips SemVer build metadata.

The `holyglory` deployment `be8103fc-42a4-4665-b3d8-1d0ae960456c`
preserves `0.147.0` as its exact binary rollback release. Its first-run legacy
account migration completed with healthy private-storage checks and a local,
owner-private, byte-verified rollback credential copy under
`.codex/migration-backups/0.149.1+multi.1/`. Account-service limits, MCP
inventory, app-server reconnection, live model turns, and usage capture were
verified without exposing credential contents.

The `holyglory` deployment `0e2c1b25-1faa-44f8-bf1d-7d3f124025a5`
atomically activated `0.150.1+multi.1` and preserves the exact
`0.149.1+multi.1` launcher and code-mode-host hashes for rollback. The live
gate verified the installed binary and all reconnected app-server processes
resolve to the new release, usage storage migrated through all four migrations
with healthy integrity and no incomplete operations, and a real model request
started, completed, and appeared as a completed `model_request` in local usage
accounting. The recorded `holygloryTT` and `axel` launcher hashes remained
unchanged; `slawa` was not targeted and its running app server was not stopped.

The `holyglory` deployment `a54f74fe-9f2d-4161-8e8a-4879d2612d86`
atomically activated `0.150.1+multi.2` and preserves
`0.150.1+multi.1` as the immediate rollback release. Tag
`v0.150.1-multi.2` identifies the tested release commit. The complete Rust
workspace passed all 16,165 tests using the checksum-verified Codex GNU V8
pair and the descriptor limit required by the Linux FD-mount fixtures; package
smoke, installed component hashes, and reconnected app-server paths also
passed. A live automatic account switch delivered the complete response before
the switch notice. A fresh DesignDocEngine capture assigned 26,186 provider
`total_tokens` to agent-declared `testing/verification_review`, preserved the
genuinely undeclared 26,153 tokens as `Unknown`, and reconciled exactly in both
the native report and DevCoordinator2. Warm all-history reads complete in
3.53–4.48 seconds; the first cold read on the 1.14 GB collector took 125
seconds and remains a separately tracked performance improvement. Only
`holyglory` was targeted; `slawa`, `holygloryTT`, and `axel` were not modified.

The `holyglory` deployment `6777849d-e475-41fe-be85-a7b276728f00`
atomically activated `0.153.0-alpha.6+multi.4` and preserves
`0.150.1+multi.2` as the immediate rollback release. Tag
`v0.153.0-alpha.6-multi.4` identifies the tested source commit. The complete
Rust workspace passed all 17,352 tests with 31 intentional skips and one
retry-resolved SQLite-startup race; the MUSL package smoke passed all seven
applicable checks with two non-Linux checks skipped. The remote UI reconnected,
both app-server processes and the code-mode host resolved through the new
release, account integrity passed all five checks, and usage storage reported
integrity `ok` with five migrations. The post-handoff doctor reported 91
incomplete usage operations, which remain truthful historical gaps; no token
count or terminal state was synthesized. Only `holyglory` was targeted, and the
recorded launcher/current-link modification times for `slawa`, `holygloryTT`,
and `axel` all predate this deployment.

## 19. VPS Collaboration Model

The server accounts `holyglory`, `holygloryTT`, `axel`, and `slawa` are operated
by the same owner and are intentionally not security-isolated from one another
for this project.

The `/home/CodexMulti` directory should use the existing `vps-repo-users`
collaboration group with:

- Setgid directory behavior so new entries inherit the shared group.
- Read, write, and traverse access for all four accounts.
- Default ACLs so future descendants remain collaboratively writable.
- No access for unrelated users.

Source-controlled credential files are prohibited regardless of this shared
access model.

## 20. Logging and Diagnostics

Add structured diagnostics for:

- Selected account alias or redacted stable profile reference
- Selection scope: default, process pin, or automatic
- Registry generation
- Account switch reason
- Login flow status without URLs containing sensitive state
- Rate-limit classification and reset time
- Credential backend type without credential content
- Migration outcome
- Usage-database schema and health
- Usage event type, coverage state, and safe categorical provenance
- Usage-capture or classification failure without captured content

Never log:

- Access tokens
- Refresh tokens
- API keys
- Authorization headers
- Full `auth.json`
- Device authorization codes
- OAuth callback query strings
- User, system, or developer prompt content
- Model output or reasoning content
- Source text, diffs, full local paths, or repository remote URLs
- Shell commands, arguments, environment values, stdout, or stderr
- Tool arguments, tool results, browser contents, or raw error messages

Provide a diagnostic command that reports compatibility-relevant state safely:

```text
codex account doctor [--json]
```

It should validate registry consistency, active-account resolution, credential
backend availability, permissions, duplicate metadata, and app-server account
state without printing secrets.

## 21. Local Usage Accounting and Attribution

### 21.1 Required outcome and scope

The customized build must contain a local stats engine that durably records and
reports model-token usage and tool activity for every operation it starts after
the feature is installed. In this document, a user-facing "chat" is an upstream
Codex thread; turns, model requests, tool calls, and delegated-agent threads are
linked children of that chat.

The authoritative local scope is one database per `CODEX_HOME`. It aggregates
all accounts, chats, agents, and repositories used by that Codex installation.
It does not silently merge separate Unix users' `CODEX_HOME` databases. A
cross-user reporting service or shared database is not part of the approved
scope and requires a separate access, privacy, backup, and operations decision.

The stats engine must support:

- Detailed drill-down from installation to repository, chat, turn, agent,
  operation, model request, token category, and tool invocation.
- Additive totals by repository and across all repositories without double
  counting multi-repo work or concurrent agents.
- Filters by time, account, model, client origin, root or delegated agent,
  phase, activity, activity state, tool, result, provenance, and coverage.
- Historical reports after a chat is archived, a checkout is unavailable, an
  account alias changes, or an account is removed.
- Explicit representation of retries, failures, interruptions, rework,
  reroutes, and accounting overhead.
- Stable machine-readable pagination and export without requiring callers to
  parse rollout files, OTel logs, or human-readable terminal output.

Service-provided account totals and daily buckets must never be allocated back
to individual chats or repositories. They do not provide enough evidence for
that attribution. Historical activity from before local capture began remains
outside coverage unless an exact provider-native per-operation record can be
imported and identified as imported.

### 21.2 Capture boundary

Implement capture in the shared core event path used by CLI, TUI, `exec`, SDK,
MCP server, and app-server execution. Do not build the database by scraping UI
text, JSONL presentation output, or optional OTel exports.

For each model-request attempt, capture:

- Stable local request and attempt IDs
- Thread, turn, root/delegated agent, operation, and repository attribution
- Account profile reference, authentication mode, model, and client origin
- Start and completion timestamps, monotonic duration, status, and safe error
  category
- Every provider-reported token category exactly as received
- Retry, reroute, compaction, continuation, and rework linkage
- Measurement provenance and coverage status

Provider-native categories must remain separate, including input, cached
input, output, reasoning output, and any future or provider-specific category.
Unknown categories are stored by versioned safe key rather than dropped.
`null` or unknown remains unknown. A derived total may be shown only when its
formula and included categories are explicit; never invent billable usage or
collapse unavailable categories into zero.

Keep the upstream fixed `TokenUsage` structure for compatibility, but add a
parallel validated provider-usage representation for accounting. It preserves
field presence, bounded future numeric category keys, failed/incomplete
response metadata when supplied, and image-generation usage that upstream
v0.149.0 currently discards. Search, realtime, memory-summary, arbitrary MCP,
and other nested providers without exact returned usage record token coverage
as unknown, never zero.

For each tool invocation, capture:

- Built-in, shell, MCP, plugin/app, browser, image, dynamic, and collaboration
  tool kind
- Safe stable tool name and a normalized operation family
- Thread, turn, agent, operation, repository attribution, and activity state
- Start and completion timestamps, monotonic duration, terminal status,
  approval outcome, and safe error category
- Retry/rework linkage and classification provenance

Do not store the command, arguments, environment, working directory, prompt,
tool payload, result, output snippet, diff, URL, browser content, or raw error.
For shell execution, store a reviewed categorical family such as `build`,
`test`, `version_control`, `package`, `deploy`, `filesystem`, `network`, or
`other`, not the command text. For MCP and plugin/app tools, the stable server,
plugin, connector, and action names may be stored only after validation against
the metadata allowlist; all call content remains excluded.

Provider-hosted tools may execute before the client receives their response
item, so a literal separate durable tool-start event is impossible. The
durably-started parent model-request attempt is their covering start; append an
observed hosted-tool row when the item arrives and disclose that provenance.
Client-invoked local, MCP, plugin/app, dynamic, browser, image, collaboration,
and shell tools still require their own durable start before dispatch.

### 21.3 Agent activity protocol

Add a tiny, local, approval-free built-in tool named `usage_activity`. It is a
stats-engine control, not a shell command or external integration. The agent is
instructed to call it before substantive work begins and whenever the current
token-spending category changes.

Each new turn and delegated agent starts as `unattributed/unknown` until its
first successful `set`; activity state is never inherited implicitly from a
parent agent or earlier turn.

Suggested v1 calls:

```json
{ "action": "set", "phase": "implementation", "activity": "coding" }
{ "action": "heartbeat" }
{ "action": "end" }
{ "action": "correct_classification", "target_id": "<safe-id>", "phase": "reporting", "activity": "verification_review" }
```

All fields are enums or bounded identifiers; the tool accepts no free-form
description. `set` is idempotent for the current agent and operation. A
heartbeat confirms an unusually long unchanged span and does not create a new
semantic category. The runtime may request a heartbeat after a configurable
time or request boundary, but the agent must not emit periodic calls when no
fresh evidence is needed.

Category changes use these semantics:

1. The agent emits a short, tool-only `usage_activity set` response before the
   target work.
2. The stats engine records the tool call and its model-response tokens as
   `reporting/accounting_overhead`.
3. After the tool succeeds locally, the declared phase/activity applies to the
   next substantive model request and subsequent tools for that agent.
4. A later `set` closes the previous span and starts the next one; `end` closes
   the span without selecting a replacement.
5. A category change is never applied retroactively to a response whose token
   usage was already produced. If substantive output accompanies the set call,
   that response retains its previous or mixed classification.

The complete provider-reported usage for the extra boundary response is
measured `accounting_overhead`. Merely offering the `usage_activity` tool also
adds some schema context to other requests, but providers do not necessarily
report that marginal cost separately from total or cached input. Report that
schema contribution only when directly measured; otherwise leave its separate
amount unknown rather than subtracting an estimate from other categories.

The activity tool produces `agent_declared` attribution, not a provider
measurement. Deterministic runtime evidence may corroborate it. For example,
an actual test runner invocation is deterministically a testing tool operation
even if the surrounding model request was declared as coding. Conflicts are
retained as separate evidence and surfaced by `usage doctor`; they are not
silently overwritten.

Add a separate bounded `usage_stats` built-in for report access. It returns the
same versioned content-free summary dimensions as the CLI and app-server and
pages repository, tool, activity, and event detail plus every approved stored
record family: processes without OS PIDs, threads, turns, agents, operations,
model/tool attempts, tokens, approvals, repository attribution,
classifications, coverage, activity spans, lifecycle events, repository
identity/events, and taxonomy versions. Each response has a hard size cap. The
tool accepts only bounded identifiers, enums, filters, cursors, and limits; it
cannot export files or read the SQLite database directly. Historical
classification correction stays on `usage_activity` so its existing phase and
activity enum schema is reused instead of duplicating context.

`usage_stats` also exposes one bounded management aggregate for a recorded task
tree:

```json
{
  "action": "task_tree_summary",
  "root_thread_id": "current",
  "include_descendants": true,
  "from_at_ms": 0,
  "to_at_ms": 4000000000000
}
```

Both time bounds and the descendant choice are required. The result reports
effective and raw operation counts, provider `total_tokens` and interval-union
wall time by active agent, expected wait expiry separately from failed waits,
content-free estimates of model-visible context for policy, conversation, and
tool-output sources, and first-pass work separately from operations linked through an
explicit `rework_of` lineage. A wrapper is deduplicated only when its recorded
execution group contains a nested tool, and a provider token fact is
deduplicated only through its factual request/tool owner and source event.
Missing historical context, incomplete intervals, unlinked wrapper/nested
operations, and absent provider totals remain explicit rather than estimated.

Add a bounded credential-free `account_management` built-in for nonsecret
routing control. It lists aliases, enabled/authenticated/default state,
priority, automatic-selection policy/order, and explicit service-limit
availability, and it can update one or all priorities through registry CAS.
It never returns email, credentials, service/workspace identifiers, login
material, or free-form notes, and it cannot delete profiles or credentials.

### 21.4 Classification taxonomy

Version the taxonomy independently of the database schema. Store three
separate axes:

1. `phase`: the broad delivery phase.
2. `activity`: the specific operation being performed.
3. `activity_state`: whether time was active or waiting.

Required phase values:

```text
planning
implementation
testing
deployment
reporting
unattributed
```

Required activity values include:

```text
requirements
specification
repository_analysis
research
diagnosis
architecture_design
work_planning
coding
configuration
refactoring
dependency_or_build_change
test_authoring
documentation_authoring
data_or_schema_change
build_validation
unit_testing
integration_testing
browser_qa
compatibility_testing
migration_rehearsal
verification_review
packaging
deployment
rollback
runtime_operations
monitoring
user_elaboration
status_update
completion_handoff
review_feedback
coordination
accounting_overhead
mixed
unknown
```

Required activity-state values:

```text
model_active
tool_active
external_wait
user_wait
blocked_wait
```

Taxonomy aliases may improve display but must resolve to one versioned canonical
value. Adding or merging categories requires a migration mapping; historical
rows keep their original taxonomy version. A report may show a normalized view
only when it discloses the mapping.

### 21.5 Measurement and attribution provenance

Measurement and attribution are independent. Every relevant row must identify
its provenance from a versioned enum that includes:

```text
provider_reported
runtime_observed
agent_declared
deterministic_classification
inferred_classification
user_corrected
imported
unknown
```

Provider-reported token counts are measurements. The operation label attached
to those counts may be agent-declared, deterministic, inferred, corrected, or
unknown. Reports must not present an inferred category as measured fact.

One provider response may contain reasoning, commentary, a tool request, or
multiple intentions while exposing only one usage object. The fork must not
split that usage proportionally between activities. Attribute the complete
response to the active activity span when evidence supports one category;
otherwise use `mixed` or `unknown`. Tool invocations retain their own exact
categorical rows. Tokens consumed inside an external tool or nested model are
counted only when that provider returns an exact usage record that can be
linked to the invocation.

Manual corrections append a `user_corrected` classification event referencing
the superseded attribution. They do not rewrite or delete the original
measurement or declaration.

### 21.6 Local database and logical schema

Use SQLite as the authoritative embedded database at:

```text
${CODEX_HOME}/usage/usage.sqlite3
```

Use a private parent directory and database permissions equivalent to
credential metadata, SQLite WAL mode on supported local filesystems, foreign
keys, a bounded busy timeout, explicit transactions, integrity checks, and
versioned forward migrations. Do not place a usage database inside a source
repository. No automatic retention expiry or aggregate-only compaction is
allowed; detailed history is retained until an explicit future retention or
deletion policy is approved.

The logical schema must include at least:

- `schema_migrations`: applied database and taxonomy versions.
- `repositories`: privacy-preserving repository key, safe display label,
  identity source, and first/last-seen timestamps.
- `threads`: local/upstream thread reference, source client, parent/fork link,
  primary repository, lifecycle, and coverage.
- `turns`: thread link, selected account snapshot, lifecycle, and coverage.
- `agents`: root/delegated identity, parent agent, role, and lifecycle.
- `operations`: immutable start/terminal events, phase/activity/state,
  provenance, status, and retry/rework/supersession linkage.
- `model_requests`: request attempts, model/account/client metadata, timing,
  status, and response linkage.
- `token_observations`: provider-native category, nonnegative count, unit,
  source request/event, provenance, and coverage.
- `tool_invocations`: safe tool identity/family, lifecycle, timing, approval,
  status, and attribution.
- `repository_attributions`: primary, observed-CWD, file-change, multi-repo, or
  unknown evidence without raw paths.
- `classification_events`: declarations, heartbeats, deterministic/inferred
  evidence, corrections, conflicts, and taxonomy version.
- `coverage_events`: capture start, complete, partial, unknown, corrupt,
  unavailable, and recovery evidence.

Authoritative facts are append-only. Terminal, correction, recovery, and
supersession events reference earlier rows. Derived SQL views may expose the
current projection and aggregates, but mutable cached totals are never the sole
record. Payload columns must be typed or allowlisted; an unrestricted JSON blob
that could capture prompts or tool payloads is prohibited.

### 21.7 Repository and multi-repo attribution

Generate a private installation-local HMAC key. Derive a repository key from a
normalized Git remote identity when present, otherwise from the Git common
directory, and otherwise from the canonical workspace root. Store only the
HMAC and identity-source kind, never the raw remote URL or full path. Default
the display label to a sanitized final directory name and allow a nonsecret
user-selected alias.

Treat linked Git worktrees as the same logical repository when the verified Git
identity permits it. If a repository moves or its remote changes and identity
cannot be proven, create a new identity rather than silently merging history;
a later explicit merge appends reconciliation evidence.

A chat can involve more than one repository. Tool operations may be attributed
to the repository proven by their runtime scope. Model-response tokens use one
additive bucket: a single proven repository, `multi_repo`, or `unknown`. Do not
copy the same token observation into every involved repository. Reports may
list all involved repositories separately from additive attribution totals.

### 21.8 Time and concurrency accounting

Store UTC wall-clock timestamps and monotonic durations where the process can
measure them. Report separately:

- Request-to-delivery wall time
- Execution wall time
- Phase interval unions
- Activity-state duration
- Tool duration
- Summed per-agent active time

Deduplicate overlapping intervals for wall-time and phase-union reports.
Concurrent agent spans may be summed only in the explicitly labelled
per-agent-active measure. Token counts from every linked agent are additive and
must not be deduplicated. Reports must not sum overlapping wall time, phase
unions, tool time, and agent time into a fictitious total duration.

### 21.9 Durability, coverage, and failure behavior

Before starting a model request or client-invoked tool, commit its start event.
If the database cannot durably accept that event, do not start the operation.
Account listing, diagnostics, and recovery commands that do not consume model
tokens or invoke external tools may remain available.

After an operation finishes, commit its terminal event and available usage. If
that commit fails, pause before the next model request or tool invocation,
retry the bounded write safely, and present an accounting failure. The durable
start event remains evidence of an incomplete operation. On restart, recovery
marks unmatched starts as interrupted with partial or unknown coverage; it
must never synthesize missing token counts.

Use stable event and request IDs so replay, resume, reconnect, duplicate stream
events, and crash recovery cannot double count. Cumulative upstream usage
notifications must be checkpointed and converted to deltas only when the
source semantics are verified. Prefer the provider response-completion usage
record as the request fact; app-server thread totals are reconciliation
evidence, not an additional additive measurement.

Corruption or migration failure must preserve the original database and WAL
for recovery. `codex usage doctor` reports integrity, migrations, incomplete
operations, classification conflicts, coverage gaps, and reconciliation
differences. No report may claim complete coverage while a related gap is
active.

### 21.10 Privacy and trust boundaries

The usage database is categorical metadata, not a content archive. It must
never contain:

- User, system, developer, or delegated-agent prompt text
- Model output, reasoning text, summaries, or message bodies
- Source content, patches, diffs, file contents, full local paths, or raw
  repository remotes
- Commands, command arguments, environment values, stdout, or stderr
- Tool arguments, results, output snippets, browser contents, URLs, query
  strings, or raw errors
- Access/refresh tokens, API keys, authorization material, device codes,
  credential files, email addresses, or opaque service/workspace identifiers

Store the stable local account profile reference needed for attribution and
resolve a current alias through the account registry at query time. When an
account no longer exists, show a redacted local fingerprint. Usage history must
not block credential deletion or recreate deleted account metadata.

The database has no network listener and no remote exporter. Upstream OTel and
analytics settings remain independent; enabling prompt logging in OTel must not
change the local database allowlist. Exports are explicit user actions, inherit
private file permissions, and pass through the same redaction validator.

These controls apply the confirmed single-owner/local-user boundaries recorded
in `security-assumptions.md`. Any shared cross-user database, network service,
external exporter, raw-content capture, additional operator, or different host
trust model is a review trigger before implementation.

### 21.11 Reporting semantics

Every report starts with:

1. Selected scope and time range.
2. Coverage state and any gaps.
3. Provider-native token totals.
4. Classification breakdown with provenance.
5. Tool counts, outcomes, and durations.
6. Time and agent concurrency measures.

Reports must identify `accounting_overhead` so the user can see the token and
tool cost of classification itself. Detailed views link each aggregate to the
underlying safe operation and observation IDs. Overall all-repository totals
include the single-repository, `multi_repo`, and `unknown` buckets exactly once.

The CLI and app-server JSON schemas must state aggregation formulas, nullable
fields, taxonomy version, database schema version, coverage, provenance, and
whether a value is measured or derived. Human-readable rounding must not alter
machine-readable integer counts.

### 21.12 Usage-accounting acceptance criteria

Usage accounting is ready only when:

1. Every model request and client-invoked tool begins with a durable database
   event; every provider-hosted tool links to its durably-started covering model
   request and is marked observed-after-execution.
2. Exact provider token categories reconcile for complete fixture and live
   test chats without double counting.
3. The agent activity tool creates bounded, visible accounting overhead and
   correctly labels subsequent spans.
4. Per-chat details reconcile to repository and all-repository aggregates,
   including concurrency, forks, retries, multi-repo work, and unknowns.
5. Every tool type and terminal outcome is represented without stored payloads.
6. CLI, TUI, and app-server reports expose coverage and provenance before
   totals and stock clients remain compatible.
7. Crash, duplicate-event, corruption, migration, and disk-full tests do not
   fabricate, lose silently, or double count usage.
8. Redaction fixtures prove prohibited content cannot enter the database,
   exports, diagnostics, logs, snapshots, or RPC responses.
9. No request-related unresolved entry remains in the authoritative completion
   ledger database.

## 22. Recovery Behavior

Document and test recovery for:

- Corrupt account registry
- Missing active account
- Missing credential profile
- Expired or revoked refresh token
- Keyring unavailable
- Interrupted migration
- Failed atomic rename
- Account removed while a process is pinned
- Upstream schema incompatibility
- Remote client rejection after upgrade
- Corrupt or locked usage database
- Interrupted usage-database migration
- Duplicate or out-of-order provider usage event
- Incomplete model/tool operation after process termination
- Disk exhaustion before or after an operation
- Repository identity collision or unverifiable move
- Classification conflict or missing activity boundary

Recovery must preserve valid profiles. A corrupt index must not cause Codex to
delete credential files automatically.

Usage recovery must preserve the original database and WAL, retain durable
start events, mark unverifiable measurements as partial or unknown, and refuse
new model/tool operations until required capture is healthy. Recovery must
never synthesize token counts, delete historical events, or treat account-level
service totals as a per-chat repair source.

## 23. Implementation Phases

### Phase 1: Baseline and compatibility harness

- Fork upstream.
- Reproduce the official build.
- Run upstream tests.
- Capture protocol schemas.
- Prove the official binary connects to the current remote Codex App.

Exit criterion: unmodified fork commit is reproducibly equivalent to the
selected upstream release.

### Phase 2: Account registry and storage

- Implement metadata registry.
- Implement profile-aware file and keyring storage.
- Implement atomic updates and locking.
- Implement legacy migration.
- Add unit and concurrency tests.

Exit criterion: multiple fake credential profiles can be stored, refreshed,
selected, migrated, and removed without cross-profile corruption.

### Phase 3: CLI management

- Implement `codex account` commands.
- Preserve `login` and `logout` compatibility.
- Add stable JSON output and exit codes.
- Add `--account` process pinning.

Exit criterion: scripts can manage profiles end to end without reading secret
files directly.

### Phase 4: Core and app-server integration

- Make `AuthManager` profile-aware.
- Snapshot accounts per turn.
- Observe safe global selection changes.
- Preserve singular upstream account RPC behavior.
- Add capability-gated extension RPC methods.

Exit criterion: unmodified app-server clients operate normally while enhanced
clients can manage multiple accounts.

### Phase 5: Local usage-accounting core

- Implement the private SQLite database, migrations, and integrity tooling.
- Capture model-request, provider-token, tool, repository, agent, and coverage
  events in the shared core path.
- Implement the versioned phase/activity/state taxonomy.
- Implement the `usage_activity` boundary tool and accounting-overhead capture.
- Add CLI JSON reports and reconciliation/redaction/concurrency tests.
- Add capability-gated app-server reads after the local schema is stable.

Exit criterion: complete fixture and live test chats reconcile from individual
requests to per-chat, per-repository, and all-repository totals with explicit
coverage/provenance and no prohibited content in the database or exports.

### Phase 6: TUI integration

- Add slash commands.
- Add selector and account details.
- Reuse official login UI and device-code flow.
- Add limits and automatic-selection controls.
- Add current-chat-first usage reports and drill-downs.

Exit criterion: all account workflows are available from the customized TUI
without exposing secrets, and detailed current-chat usage is available without
exposing captured content.

### Phase 7: Automatic selection

- Implement deterministic priority policy.
- Implement safe pre-turn selection.
- Handle reached-limit classifications.
- Prevent unsafe replay after side effects.

Exit criterion: limit exhaustion selects another eligible account at a safe
boundary and reports no-eligible-account truthfully.

### Phase 8: Release and VPS deployment

- Complete compatibility matrix.
- Package release and rollback artifacts.
- Stage on the VPS.
- Migrate one test account first.
- Keep the verified `slawa` deployment and rollback evidence intact.
- Validate binary activation under `holyglory` without reading credentials.
- Replace only the `holyglory` current-release link and retain its existing
  launcher.
- Verify CLI resolution and package hashes under `holyglory`.
- Complete first-run account migration only with a private, verified legacy
  credential rollback copy, then verify account and usage integrity.
- Prove the `slawa`, `holygloryTT`, and `axel` installations were not changed.

Exit criterion: supported `slawa` and `holyglory` callers use the customized
`codex` binary with exact per-account rollback evidence and no request-related
unresolved compatibility issue, while `holygloryTT` and `axel` remain
unchanged.

## 24. Definition of Done

The project is complete only when:

1. Existing official CLI workflows pass against the fork.
2. Existing app-server protocol clients remain compatible.
3. The current remote Codex App connects and completes real tasks.
4. All account CRUD and authorization workflows work in CLI and TUI.
5. External scripts have stable JSON commands or RPC methods.
6. File and keyring credential stores support independent token refresh.
7. Legacy single-account authentication migrates without reauthorization or
   data loss.
8. Manual global and process-local switching work concurrently.
9. Automatic selection works at safe turn boundaries.
10. Unsafe turn replay is prevented.
11. No account token or credential appears in source, logs, fixtures, output,
    or account-management UI.
12. Upstream sync, schema comparison, build, test, package, deployment, and
    rollback procedures are documented and exercised.
13. All four VPS users can build and maintain the project under
    `/home/CodexMulti`.
14. Every post-install chat, turn, agent, provider-token category, and tool
    invocation is durably represented with coverage and attribution provenance.
15. Detailed chat reports reconcile to repository and all-repository totals
    without concurrency, retry, fork, or multi-repo double counting.
16. The activity-boundary tool attributes subsequent work and exposes its own
    token/tool overhead separately.
17. Usage database, reports, exports, diagnostics, logs, and RPCs contain none
    of the prohibited content listed in Section 21.10.
18. Required capture, crash recovery, corruption handling, and migrations are
    exercised on every supported platform and filesystem.
19. No in-scope unresolved item remains in the authoritative software-owned
    completion-ledger database.

## 25. First Engineer Checklist

Begin in this order:

1. Read this handover completely.
2. Read upstream `README.md`, `CONTRIBUTING.md`, license, notices, and Rust
   workspace instructions at the selected revision.
3. Record the initial upstream tag and commit.
4. Create the fork remotes and branch policy.
5. Build the unmodified upstream binary on the VPS.
6. Run the upstream test suite before changing code.
7. Generate and archive the base app-server TypeScript and JSON schemas.
8. Verify remote Codex App compatibility with the unmodified build.
9. Inspect current auth manager and storage tests.
10. Inspect current provider-usage, item/tool event, delegated-agent, OTel, and
    app-server token-usage paths without treating optional OTel as the local
    database source of truth.
11. Establish the reviewed software-owned completion-ledger database interface
    before implementation work creates unresolved items.
12. Write the account registry and migration acceptance tests before modifying
    credential persistence.
13. Write usage-accounting reconciliation, crash, concurrency, and redaction
    acceptance tests before adding capture to the model/tool loop.
14. Implement Phase 2 without adding TUI behavior prematurely.
15. Implement Phase 5 storage/capture before adding `/usage` presentation.
16. Keep credentials and prohibited usage content out of every development
    artifact.

## 26. Key Design Decisions

The following decisions are approved and should not be reopened without new
evidence:

- Build a real downstream Codex CLI, not a wrapper or proxy.
- Preserve the executable name `codex` and upstream external behavior.
- Use a maintained fork for source modifications.
- Preserve singular app-server account methods as active-account views.
- Add multi-account RPCs only as additive, capability-gated extensions.
- Store each account's credentials independently.
- Keep turns pinned to an immutable account snapshot.
- Support both global-default and process-local account selection.
- Perform automatic selection in the CLI before a turn, not in the model.
- Do not replay possibly side-effecting turns automatically.
- Keep account displays and logs free of credential material.
- Keep a required, append-only, private SQLite usage database per `CODEX_HOME`.
- Capture usage in the shared core path, independently of optional OTel export.
- Preserve provider-native token categories and unknowns instead of inventing
  semantic splits, billable totals, or zero values.
- Use a short `usage_activity` tool at activity boundaries; classify its result
  as agent-declared and expose its own accounting overhead.
- Preserve upstream `/usage daily|weekly|cumulative` behavior and extend the
  existing bare `/usage` menu with local current-chat statistics.
- Use distinct singular-resource `accountProfile*` and `localUsage*` app-server
  namespaces; never overload service-backed `account/usage/read`.
- Reject `--account` when an external/environment auth owner would silently
  override it; never pretend the requested profile is active.
- Keep measurement provenance separate from operation-attribution provenance.
- Aggregate multi-repo and concurrent work without duplicating tokens or
  summing overlapping intervals as wall time.
- Store categorical metadata only; do not turn usage accounting into a prompt,
  output, command, tool-payload, source, or credential archive.

Any proposed departure must document the evidence, compatibility impact,
migration impact, security impact, and upstream-maintenance cost before code is
changed.
