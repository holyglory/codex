<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you want the desktop app experience, run <code>codex app</code> or visit <a href="https://chatgpt.com/codex?app-landing-page=true">the Codex App page</a>.
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

### Installing and running Codex CLI

Run the following on Mac or Linux to install Codex CLI:

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | sh
```

Run the following on Windows to install Codex CLI:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"
```

The standalone installers download from `https://releases.openai.com/codex` by default and fall back to GitHub Releases if a metadata or asset download is unavailable. To force GitHub Releases, set `CODEX_INSTALLER_USE_RELEASES_OPENAI_COM` to `false` (`0` and `no` are also accepted):

```shell
curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_INSTALLER_USE_RELEASES_OPENAI_COM=false sh
```

```powershell
$env:CODEX_INSTALLER_USE_RELEASES_OPENAI_COM='false'; irm https://chatgpt.com/codex/install.ps1 | iex
```

Codex CLI can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @openai/codex
```

```shell
# Install using Homebrew
brew install --cask codex
```

Then simply run `codex` to get started.

<details>
<summary>You can also go to the <a href="https://github.com/openai/codex/releases/latest">latest GitHub Release</a> and download the appropriate binary for your platform.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `codex-x86_64-unknown-linux-musl`), so you likely want to rename it to `codex` after extracting it.

</details>

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Business, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).

## Use multiple accounts

The following examples use `personal` and `work` as local aliases. Substitute your existing aliases, or add the missing accounts first.

### Add accounts and choose a default

```sh
codex account add personal
codex account add work
codex account list
codex account use personal
codex account current
```

Each `add` opens a separate authorization flow. For a machine where a browser callback is inconvenient, use device authorization:

```sh
codex account add work --device-auth
```

Use either the normal or device-auth form for an account, not both. Finish signing in before using the new profile.

### Pin an invocation

```sh
codex --account work
codex --account work exec "Explain this repository"
codex --account work resume --last
```

A pin applies to that process without changing the saved default. It also prevents automatic selection from moving that process to another account. Changing the default affects subsequent unpinned turns; a turn already running keeps its selected account.

### Set automatic-selection priorities

Automatic selection is off by default. Higher numbers are used first; smaller numbers are kept for later. Among eligible accounts with the same priority, the account with the most time left until its main Codex quota resets is used first. The current account stays unless another account resets at least 10 minutes later. New profiles start at priority `1000`.

```sh
codex account priority set work 2000
codex account priority set personal 1000
codex account priority list
codex account auto on
codex account auto status
codex account limits --all
```

When enabled, selection considers enabled, authenticated, locally managed ChatGPT OAuth profiles and service-reported availability. API keys and other externally managed authentication modes are not silently added to the pool. Unavailable limit information stays unknown rather than being reported as free capacity.

Failover is limited to eligible failures before response content has started; it does not replay a partially completed response or completed tools. It is not a promise that every failure can continue on another account.

To return to manual selection or reset the numeric priorities:

```sh
codex account auto off
codex account priority set-all 1000
```

Apply one banked reset to a specified profile with `codex account reset work`.
The command applies immediately and selects the available credit that expires soonest;
non-expiring credits are used last. Use `--credit-id CREDIT` to choose one explicitly,
or `--json` for the outcome, windows reset, request identity and refreshed usage.

If the response is uncertain, retry the same operation with
`codex account reset work --credit-id CREDIT --request-id REQUEST`, using the
identifiers printed by the original attempt. The replay reaches the service even
when that credit has already been consumed; it does not select another credit.
Confirmed resets and explicit no-op outcomes return zero. No credit, unsupported
authentication, request failures and uncertain outcomes return nonzero. A refresh
failure is reported separately and does not undo a confirmed reset.

### Permit credit fallback per account

`codex account list` shows remaining spendable credits and whether automatic credit
use is enabled. Banked resets are separate and are never redeemed automatically.

```sh
codex account edit work --credit-usage enabled
codex account edit work --credit-usage disabled
```

Credit use defaults to disabled. With automatic selection enabled, Codex first uses
included allowance within the highest priority tier, then permitted credits in that
tier, then lower priorities. Unknown peer allowance blocks that tier's credit
fallback; a failed check is not proof of exhaustion. Available included allowance
takes precedence again after a reset. Service spending restrictions still apply.

This is a routing permission, not a backend spending cap: running turns keep their
account lease, and explicit `--account` pins and manual mode are unchanged.
Older binaries cannot read a registry while an enabled credit permission is saved;
disable credit use with this version before returning to an older binary.

### Inspect and maintain accounts

| Command                                         | Purpose                                                           |
| ----------------------------------------------- | ----------------------------------------------------------------- |
| `codex account show work`                       | Show one profile's metadata.                                      |
| `codex account limits work`                     | Fetch that account's service-reported limits.                     |
| `codex account rename work office`              | Change its local alias.                                           |
| `codex account edit work --note "Work account"` | Change a local note; `--clear-note` removes it.                   |
| `codex account disable work`                    | Keep the profile but make it unavailable for selection.           |
| `codex account enable work`                     | Make a disabled profile eligible again.                           |
| `codex account remove work`                     | Remove the profile and its stored credentials after confirmation. |
| `codex account doctor`                          | Diagnose registry and credential-storage problems.                |

These are independent examples: after renaming an account, use its new alias. Removal can be rejected while a turn or process is using that profile. Use `--json` for structured account-command output, and consult each subcommand's `--help` before scripting mutations.

### Manage existing accounts from an agent

Running Codex agents have the native `account_management` tool. It supports
`list`, `set_priority`, `set_all_priorities`, `rename`, `enable`, `disable`,
`set_default`, `set_auto_selection` and `set_credit_usage`. Read `list` first and supply its
`generation` as `expected_generation` when changing profiles. Automatic selection
uses `mode: "enabled"` or `mode: "disabled"`; `set_credit_usage` uses the same modes plus an account reference. Rename uses `new_alias`.

For live limits, call `list` with `refresh_service_usage: true` and follow
`nextOffset` to inspect every account. Each `serviceUsage` includes service reset
timestamps, `nextResetAt` and a readable `nextResetAtUtc`. The accompanying
`serviceUsageBucketFields` identifies the compact bucket columns. Responses omit
credentials, email, service/workspace identifiers and notes. Profile changes keep
the running turn's credential lease; subsequent routing uses the saved changes.
Login and profile removal remain in the owner-controlled account flows.

### Manage accounts inside the terminal UI

Enter these in the interactive Codex prompt, not your shell:

```text
/account
/account use work
/account add work
/account edit work
/account limits
/account auto
```

`/account` opens the account list; the other forms open the corresponding action or view. Clients connected to an upstream server do not gain these capabilities just because the local executable is the fork.

You can also ask the agent to list local account aliases, identify which profile handled the current turn, show available limits, or change priorities. The built-in `account_management` tool supports these bounded operations; it does not sign in, activate, rename, or remove profiles and does not reveal credentials, email, or service/workspace identifiers. Use the explicit account commands for those management actions.

## Diagnose network failures

Codex Multi keeps model HTTP and WebSocket diagnostics in a separate private
`network_diagnostics_1.sqlite` database beside its other SQLite state files
(normally in `~/.codex`). Records have no automatic expiry or per-task row cap;
ordinary debug-log pruning and runtime restarts do not remove them.

```sh
codex debug network-errors --thread-id TASK_ID --limit 20
codex debug network-errors --thread-id TASK_ID --before-id NEXT_BEFORE_ID
codex debug network-errors --thread-id TASK_ID --include-context
```

The JSON report includes UTC Unix timestamps in milliseconds, task and turn IDs,
available request/response IDs, HTTP statuses, provider error codes, WebSocket
close codes and redacted reasons, retry schedules, fallback, and completion
markers. `--since-ms` limits the time range; `nextBeforeId` continues an older page.
The default view shows incidents; `--include-context` adds successful request and
completion records around them. Incident queries use dedicated indexes, so a long
successful run does not bury earlier failures or require scanning all its activity.

Large WebSocket requests upload complete context items in ordered batches before
generating one response. The client targets 4 MiB per request; this is a batching
choice, not the server's documented limit. If an item cannot fit intact or the
server rejects preparation, Codex uses HTTP with the complete request. No images
or conversation items are discarded. The ledger records request byte counts and
batch preparation alongside recovery events.

Request/response bodies, prompts, tool output, credentials, and arbitrary headers
are excluded. Individual error/reason fields retain up to 8,192 characters, with
an explicit truncation marker; metadata fields retain up to 512. Disk write
failures are reported and retried without stopping model work. Queue overflow is
reported immediately and counted in the next retained record. The ledger grows
with network activity and is not uploaded automatically.

## Inspect local usage

Local accounting answers **what this fork observed on this machine**, not how much quota remains on OpenAI's service. Use `codex account limits` for service limits. The local collector does not reconstruct a complete usage history from before it was installed.

### Read reports

```sh
codex usage summary
codex usage repo
codex usage chat THREAD_ID
codex usage summary --account work --json
codex usage repositories --json
codex usage tools --limit 20
codex usage activities --limit 20
codex usage events --limit 20
```

Replace `THREAD_ID` with a real task/thread UUID. `repo` uses the repository in the current working directory; `repositories` lists identities known to the collector. `codex usage repo --identity-only --json` resolves the current stored repository identity without aggregating its history.

Summary reports include model-token categories, tool and model operation counts, activities, timing, and coverage. Unknown or missing measurements are not zero. Provider-measured tokens and estimates remain distinct, and summed active time across concurrent agents is not the same as elapsed time.

Use `--since` and `--until` to select a time window with RFC3339 timestamps or Unix milliseconds; the start is inclusive and the end exclusive. For example:

```sh
codex usage summary --since 2026-09-01T00:00:00Z --until 2026-09-02T00:00:00Z --json
```

This example selects one historical UTC day. Available filters and breakdowns depend on the subcommand; use `codex usage --help` and the relevant subcommand's `--help` for the supported combinations. Paginated lists accept `--limit` and the returned `--cursor` for the next page.

### Read reports while working

In the terminal UI:

```text
/usage all
/usage chat
/usage repo
/usage tools
/usage activities
/usage events
```

Use an explicit scope for the fork's local reports. The existing `/usage` service view and its `daily`, `weekly`, and `cumulative` variants are separate from local accounting.

In a model-routed chat, ask for a local usage report or send `/usage all`, `/usage chat`, or `/usage repo`. These use the built-in `usage_stats` tool rather than a terminal slash-command dispatcher. For a combined parent-and-subagent view, ask:

> Summarize this task's local usage, including delegated agents, active time, waiting time, and any missing measurements.

The tool's `task_tree_summary` report accepts `root_thread_id="current"` and `include_descendants=true`. Agent activity declarations and explicit classification corrections are recorded through `usage_activity`; neither tool needs conversation content to produce the accounting report.

### Export and diagnose

```sh
codex usage export --format json --output usage-report.json
codex usage doctor
```

Exports support `json`, `jsonl`, and `csv`, write a new file, and do not overwrite an existing file. `codex usage details --help` exposes the detailed record families; `codex usage classify --help` and `codex usage repo --help` describe classification corrections and repository identity maintenance.

The accounting database is stored at `CODEX_HOME/usage/usage.sqlite3`. Its records and exports exclude prompts, responses, source code, shell commands, tool payloads, credentials, raw filesystem paths, and repository remotes. This **content-free guarantee applies to the accounting store**, not to all Codex session files or to the metadata displayed by account-management commands.

Accounting failures are reported as collection gaps and do not cancel model or tool work. A report with incomplete coverage is not a complete measurement.

## Project reviews and delivery deadlines

Project automation uses the fork's persistent local app-server. Start that server
with `codex app-server daemon start` if it is not already running; closing a client
does not remove its saved schedules. `codex project status --json` checks whether
the connected server supports project automation.

Substantive work is enrolled for performance review. Specification, discussion
and analysis do not create delivery obligations. When implementation actually
has an authorized preliminary result, bind its purpose and activate that specific
delivery target. Worktrees share their Git project identity; workstreams keep
unrelated delivery obligations separate. Use `codex project --help` for the
commands and `--thread TASK_ID` to identify the persistent task.

The default delivery deadline is 24 hours: delivery starts while independent
development continues. Only the separate 36-hour hard stop restricts the affected
implementation to delivery recovery. User-authorized postponements change the
effective deadlines without claiming a delivery happened. Explicit pauses and
completed outcomes retire active scheduling; starting a new specification does
not revive old delivery alarms.

Performance reviews run independently, including for work with nothing to deploy.
An inactive window does not invoke the model. A review investigates measured
bottlenecks, compares remedies, and records a verified keep/revert decision or a
reasoned inconclusive/no-change result. It may improve workflows inside the
reviewed repository and existing task scope; specification work never authorizes
implementing the product. Usage stays in the local collector, and Coordinator
retains the linked decisions, outcome and verification records.

Delivery and review alarms normally run only during user work. Stop defers pending
alarms and suspends existing background-wake permissions. Starting or resuming work
releases applicable missed alarms with their original deadlines. Opening or reading
a task is passive. Explicit project pauses continue to control the project clocks.

The agent's `await_work` tool waits on real event publishers instead of repeatedly
asking the model for status. Coordinator test/deployment waits use the
`devcoordinator` source with exact repository and run/deployment labels. A missing
publisher or expired event history is not treated as successful completion.

## Continue tasks on events or heartbeats

**Experimental, opt-in.** A subscription belongs to an existing persistent task and can match an event source/type, a periodic heartbeat, or both. Registration defaults to delivery during running user work. Starting an inactive task requires explicit background-wake permission; that turn consumes usage.

Enable the feature in the Codex home used by your server:

```sh
codex features enable event_subscriptions
```

Keep a fork app-server running. If you already use its local daemon, restart it to load the feature:

```sh
codex app-server daemon restart
```

If no persistent server is configured, see `codex app-server daemon --help` and the [app-server guide](codex-rs/app-server/README.md) first. Subscription commands require a persistent local daemon or an explicitly configured authenticated remote endpoint; they do not leave a short-lived embedded server running for future deadlines.

For a real task UUID, subscribe to build completion and also check every 15 minutes:

```sh
codex subscriptions create --thread THREAD_ID --source build --event-type completed --label branch=main --heartbeat-seconds 900
codex subscriptions list --thread THREAD_ID
```

Inspect wake permissions and pending alarms with:

```sh
codex subscriptions wake-policy --thread THREAD_ID
```

Only following an explicit user request, use the returned revision to allow a
particular subscription to wake an inactive task:

```sh
codex subscriptions wake-policy --thread THREAD_ID --subscription SUBSCRIPTION_ID --policy allow-background --expected-revision REVISION --authorization-ref USER_REQUEST
```

Omit `--subscription` for a task-wide grant. Project alarms can instead use
`--project PROJECT_ID --target TARGET` for one delivery target, or
`--project PROJECT_ID --review` for its review timer. Specific policies take
precedence over the task-wide policy. `--policy running-only` restricts a scope
to running user work. Ordinary clock registration and standing review/delivery
cadences do not authorize background grants.

Stop suspends existing grants until the user resumes work. Fresh permission while
stopped enables its requested scope. Handling one permitted alarm keeps unrelated
stopped work and alarms deferred. Pending alarms and permissions survive restart;
missed intervals coalesce, and obsolete or already delivered alarms are retired.

Your build integration must publish an event after the build actually completes; creating a subscription does not connect a build service automatically:

```sh
codex subscriptions publish --source build --event-type completed --sequence 42 --label branch=main
```

`42` is an example sequence number. Publishers must advance the source sequence for subsequent events; duplicate and out-of-order sequences are ignored for each subscription. Use the same matching source, event type, and labels as the subscription.

For a heartbeat-only subscription, omit `--source`, `--event-type`, and `--label`. To request an immediate continuation or cancel future wakes, substitute an ID returned by `subscriptions list`:

```sh
codex subscriptions trigger --id SUBSCRIPTION_ID
codex subscriptions cancel --id SUBSCRIPTION_ID
```

Due alarms are combined and join running user work at a safe input boundary.
An explicit `trigger` authorizes immediate handling of the selected subscriptions;
Stop cancels pending immediate triggers while preserving timed/event alarms.
Event ingress uses the existing app-server connection and bounded typed metadata.
See the [event and heartbeat API example](codex-rs/app-server/README.md#example-subscribe-a-thread-to-events-and-heartbeats-experimental) for authenticated remote integrations.

## Update Codex Multi

### Published npm installation

Use the fork's package name explicitly:

```sh
npm view @holyglory/codex version
npm install -g @holyglory/codex@latest
codex --version
```

Close and relaunch running CLI sessions after updating. An already running app-server also needs to be restarted through the mechanism that owns it; changing files on disk does not replace a process already in memory.

Published fork builds also support `codex update` for recognized package-manager installations and target `@holyglory/codex@latest`, not `@openai/codex`. For an older fork build whose updater predates that behavior, use the explicit npm command above. Homebrew, standalone, and manually deployed builds do not have a fork-hosted automatic update path; update them through their original delivery method or switch to npm.

To install a particular published version instead of the current npm release:

```sh
npm view @holyglory/codex dist-tags --json
npm view @holyglory/codex versions --json
npm install -g @holyglory/codex@VERSION
```

Replace `VERSION` with a published launcher version, not a platform-suffixed package version. npm's `latest` tag is the release channel; a newer source commit or successful candidate workflow does not by itself mean a new package has been published.

### Development checkout

If you want to change the fork rather than use the packaged release:

```sh
git clone https://github.com/holyglory/codex.git
cd codex/codex-rs
cargo build --locked --bin codex
cargo run --locked --bin codex -- --help
```

Install Rust through rustup and your platform's native build prerequisites first. Use the toolchain pinned in `codex-rs/rust-toolchain.toml`; the build may download additional native dependencies. This creates a development build, not a global installation or a complete distributable package. See the [package assembly guide](scripts/codex_package/README.md) when packaging companion executables and resources.

To update a clean checkout, run `git pull --ff-only` from its repository directory, then rebuild with the same commands. If the pull refuses because of local changes or divergence, preserve and reconcile those changes; do not reset them just to follow an update recipe. Rebuilding a checkout does not update an npm installation.

## Clients and further reference

- **[App-server API and examples](codex-rs/app-server/README.md):** compatible clients must detect `multiAccount`, `localUsageAccounting`, and, when enabled, `eventSubscriptions` in the initialization response and opt in to the experimental extension APIs.
- **[Upstream Codex documentation](https://developers.openai.com/codex):** reference for shared coding, configuration, authentication, and tool workflows. Its installation instructions target standard OpenAI Codex, not this fork.
- **[Contributing](docs/contributing.md):** development guidance; also read the applicable `AGENTS.md` instructions before changing code.
- **[Issues](https://github.com/holyglory/codex/issues):** report problems with this distribution here.

Installing this package does not add a graphical multi-account manager to the unmodified OpenAI desktop app or IDE extension. Fork-specific behavior depends on the connected server and what that client exposes.

Licensed under [Apache-2.0](LICENSE), with upstream and bundled-component notices in [NOTICE](NOTICE).
