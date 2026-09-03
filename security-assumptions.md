# Security Assumptions

Last reviewed: 2026-09-03

## Confirmed project context

- **Users and operators:** All Unix accounts on this machine are controlled by
  the same owner for this project. The accounts `holyglory`, `holygloryTT`,
  `axel`, and `slawa` are intentionally not security-isolated from one another
  (`HANDOVER.md`, Section 19; user confirmation on 2026-08-21).
- **Repository access:** Every participating local account has the same full
  collaborative rights to repository files through one shared group. Keep the
  repository ACL simple: no per-user access exceptions or account-specific
  repository permissions (`HANDOVER.md`, Section 19; user confirmation on
  2026-08-21).
- **Excluded users:** There are currently no unrelated human operators that
  require a separate repository boundary. If that changes, review repository
  access before granting the new account access.
- **Runtime and ownership:** The primary target is the owner-operated `vr.ae`
  development VPS, with source intended at `/home/CodexMulti`
  (`HANDOVER.md`, header and Section 19).
- **Credential sensitivity:** Access tokens, refresh tokens, API keys,
  authorization headers, device codes, OAuth callback state, and credential
  files are sensitive and must not enter source control, logs, fixtures,
  snapshots, diagnostics, or normal UI (`HANDOVER.md`, Sections 6, 16, and 20).
- **Account boundary:** Multiple configured Codex accounts belong to the
  authorized operator. Multi-account selection must not be presented or used
  as a way to bypass service entitlements or limits (`HANDOVER.md`, Section
  14.3).
- **Automatic-selection billing boundary:** Automatic selection and failover
  are restricted to locally managed ChatGPT OAuth profiles. PAT, API-key,
  Bedrock, Agent Identity, header-backed, and externally managed authentication
  remain available with automatic selection off or through explicit process
  pins (DEC-CODEX-009; user confirmation on 2026-08-22).
- **Automatic-selection availability checks:** Auto-enabled processes may make
  bounded, read-only rate-limit requests under temporary managed-ChatGPT
  profile leases before model work. Failures remain unknown, request contents
  stay content-free, and excluded authentication modes are never probed
  (DEC-CODEX-011; `UIL-BUSINESS-LOGIC-ACCOUNT-SELECTION-001`).
- **Deployment scope:** Installing the customized binary is authorized for the
  local Unix accounts `slawa` and `holyglory`. The current request adds
  `holyglory` while preserving the verified `slawa` installation. The
  launchers, current-release links, runtime data, and processes of
  `holygloryTT` and `axel` remain outside this deployment mutation boundary
  (user confirmations on 2026-08-22 and 2026-08-25).
- **Public distribution:** The operator authorizes a public source fork at
  `github.com/holyglory/codex` and a public npm package named
  `@holyglory/codex`. Published packages preserve the `codex` executable name,
  contain only reviewed release artifacts and required redistribution files,
  and must not include credentials or per-user runtime data (user confirmation
  on 2026-09-03; DEC-CODEX-016).
- **Publication authority:** The first npm release uses the operator's
  interactive, 2FA-protected npm session because npm requires a package to
  exist before trusted publishing can be configured. Subsequent submissions
  use GitHub Actions OIDC with stage-only authority; a human approves staged
  versions before they become public. Long-lived npm write tokens are not an
  accepted steady-state control (user confirmation on 2026-09-03;
  DEC-CODEX-016).
- **Authorized accounting scope:** The current request authorizes categorical
  token/tool usage accounting per chat and repository and totals across
  repositories. It does not authorize retaining the content of prompts,
  outputs, source, commands, or tool payloads.
- **Authorized in-product agent access:** The operator explicitly authorizes
  Codex agents to read the complete approved content-free usage reports, list
  nonsecret local account-routing metadata, change account priorities, and
  append bounded categorical usage-classification corrections.
  This does not authorize an agent tool to read credentials, complete login,
  delete profiles or credentials, expose email or opaque service/workspace
  identities, or broaden automatic selection beyond managed ChatGPT OAuth.

## Current trust boundaries and controls

- Source collaboration and per-user runtime data are different boundaries.
  Repository owner and shared-group permissions are equal, directories retain
  the shared group and inherit group `rwx`, and named per-user ACL entries are
  unnecessary. Credentials and the usage database remain under the owning
  user's `CODEX_HOME`.
- Local usage accounting has no network listener or automatic exporter. Its
  SQLite database and explicit exports use private filesystem permissions.
- Model-visible usage and account-management responses are structured and
  size-bounded. Detailed facts are paginated; account mutations are
  generation-aware and limited to the expressly authorized nonsecret routing
  metadata, while usage corrections remain append-only and enum/identifier
  only.
- The usage database stores allowlisted categorical metadata. It excludes
  prompts, model/reasoning output, source and diffs, raw paths/remotes,
  commands/environment/output, tool payloads/results, raw errors, credentials,
  emails, and opaque service/workspace identifiers.
- Account aliases are resolved from the account registry at query time. Usage
  history does not preserve deleted account metadata or prevent credential
  deletion.
- Environment, workload-identity, and externally refreshed credentials retain
  their upstream owner. An explicit local `--account` pin conflicts with those
  sources rather than silently overriding or being overridden by them.
- Usage accounting is observational and never blocks model or tool work. When
  its private durable store is unavailable, corrupt, or inconsistent, the
  process retains content-free pending records in a bounded in-memory retry
  cache, retries after later capture opportunities, and emits content-free
  outage, overflow, retry, and recovery logs. Exhausted cache capacity or
  process exit may leave a collection gap visible only in those logs, but
  missing facts are never synthesized and accounting failure never becomes a
  work-availability gate (user confirmation on 2026-09-03;
  UIL-BUSINESS-LOGIC-USAGE-ACCOUNTING-003).
- Public release automation builds on GitHub-hosted runners, verifies the
  complete platform package set before publication, stages platform payloads
  before the root wrapper, and keeps final publication behind npm's interactive
  2FA approval boundary.

## Unknown or out of scope

- Cross-user aggregation across the four separate `CODEX_HOME` databases is
  not approved or designed.
- A shared/networked stats service, remote dashboard, automatic exporter, or
  third-party analytics integration is not approved.
- Raw prompt, output, reasoning, command, source, browser, or tool-content
  retention is not approved.
- Additional operators, multi-tenant hosting, deployment beyond `slawa` and
  `holyglory`, external backups, retention deletion, legal/compliance
  requirements, and acceptable loss windows are not yet specified.
- Repository names may themselves be sensitive. The initial design stores a
  sanitized local label and privacy-preserving repository key, not raw paths or
  remotes; a broader naming policy remains unconfirmed.

## Review triggers

Revisit these assumptions before implementing cross-user aggregation, a
network API, external export, raw-content capture, additional operators,
multi-tenant or production deployment, a different host ownership model,
automatic retention/deletion, or any control that changes credential or usage
database access. Also revisit before broadening automatic selection beyond
locally managed ChatGPT OAuth profiles, changing the availability-probe endpoint
or its read-only/pre-turn boundary, deploying to another Unix account beyond
`slawa` and `holyglory`, or introducing an account not controlled by the current
owner. Also revisit before allowing agent tools to perform login, credential
access or deletion, profile
removal, external export, or account mutations beyond the authorized routing
metadata. Revisit before transferring the GitHub repository or npm package,
adding another trusted publisher or package maintainer, permitting direct
unreviewed publication, or introducing a persistent npm write credential.
Revisit before making usage capture a prerequisite for model or tool work,
changing the bounded in-memory retry policy, or adding any persistent fallback
outside the private usage database.
