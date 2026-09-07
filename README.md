# Codex Multi

**Codex Multi is an unofficial fork of [OpenAI Codex](https://github.com/openai/codex) for working with multiple accounts and understanding where agent time and tokens go.** It keeps the `codex` command and the familiar terminal and app-server workflows, while adding account management, local usage reports, and event-driven task continuation.

The public package is **`@holyglory/codex`**, maintained in **[holyglory/codex](https://github.com/holyglory/codex)**. It is not an OpenAI distribution.

## Why this fork exists

Using several authorized Codex accounts should not require repeatedly signing out or manually swapping credential files. Likewise, understanding the effort spent on a project should not require reading conversation logs or guessing from a single token total.

This fork adds:

- **Named account profiles:** sign in once per account, inspect available limits, choose a default, or pin one invocation to a particular account.
- **Priority-based automatic selection:** opt in to selecting eligible ChatGPT accounts before turns, with higher-priority accounts used first.
- **Content-free local accounting:** inspect tokens, tool calls, activity categories, timing, and collection gaps by task, repository, account, or across local history. Ask the agent for a task-tree report that includes its delegated work.
- **Event subscriptions and heartbeats:** let an existing task continue when a matching event arrives or a scheduled heartbeat becomes due, rather than repeatedly asking the model to poll.
- **Additive app-server APIs:** expose the account, accounting, and subscription features to compatible clients without replacing the existing single-account interfaces.

These features manage local workflows; they do not combine subscriptions, grant additional service access, or bypass OpenAI account, workspace, billing, or rate-limit rules.

## Install Codex Multi

### Requirements

- Node.js and npm. The published launcher declares Node.js 16 or newer; use a currently supported Node.js LTS release.
- A supported platform: Linux, macOS, or Windows, on x64 or ARM64. The npm package selects the corresponding native package automatically. Install inside WSL if that is where you run Codex.
- A Codex-compatible sign-in. Managed multi-account automatic selection uses ChatGPT OAuth accounts; other authentication modes are not part of that automatic pool.

You do not need Rust or a source checkout to install the published package.

### 1. Remove a conflicting standard Codex installation

Both distributions install a command named `codex`. If standard OpenAI Codex is already installed, close its running terminal sessions and remove the old CLI using the method that installed it. Skip this step on a new installation.

First locate existing commands:

**macOS / Linux / WSL**

```sh
type -a codex
npm list -g --depth=0
```

**Windows PowerShell**

```powershell
Get-Command codex -All | Select-Object CommandType, Source
npm list -g --depth=0
```

Use the applicable uninstall command, not every command in this table:

| Original installation                             | Remove standard OpenAI Codex                                                                                                                                               |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| npm                                               | `npm uninstall -g @openai/codex`                                                                                                                                           |
| Homebrew cask                                     | `brew uninstall --cask codex`                                                                                                                                              |
| pnpm                                              | `pnpm remove -g @openai/codex`                                                                                                                                             |
| Bun                                               | `bun remove -g @openai/codex`                                                                                                                                              |
| Standalone installer or manually extracted binary | Remove the old installation's executable or launcher at the path identified above. Do not remove an unrelated package-manager installation by deleting its files manually. |

If multiple installations appear, resolve each conflicting one. With a Node version manager, check the global packages for the Node installation you actually use.

**Keep your Codex data.** Uninstalling the executable is not a request to delete `CODEX_HOME` (normally `~/.codex`, or `$HOME\.codex` on Windows). That directory holds configuration, sessions, and authentication data. Back it up privately before migrating; do not use a cleanup command that deletes it.

This procedure replaces the CLI, not the separately installed OpenAI desktop app or IDE extension.

### 2. Install the fork

```sh
npm install -g @holyglory/codex@latest
```

Open a new terminal so it resolves the new executable, then verify:

```sh
codex --version
npm list -g @holyglory/codex --depth=0
codex account --help
codex usage --help
```

The native version includes a downstream `+multi.<revision>` suffix. npm represents that revision with `-multi.<revision>`, so the two version strings need not be identical. If `account` or `usage` is unknown, check the executable paths again: an old binary, alias, or wrapper may still take precedence.

Do not use `npm install -g @openai/codex`, `brew install --cask codex`, or OpenAI's standalone installer to install this fork. Those install the upstream distribution. The supported public installation path here is npm; do not assume a GitHub workflow artifact is a published release or a complete standalone installation.

### 3. Start working

From a project directory:

```sh
codex
```

On a fresh installation, follow the sign-in flow. Existing supported legacy authentication is migrated into a local account profile when the fork initializes its account registry. Check the imported alias rather than assuming it is named `default`:

```sh
codex account list
codex account current
```

Configuration and authentication profiles are different concepts: `--profile` selects a configuration profile; `--account` selects a signed-in account.

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

Automatic selection is off by default. Higher numbers are used first; smaller numbers are kept for later. New profiles start at priority `1000`.

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

## Continue tasks on events or heartbeats

**Experimental, opt-in.** A subscription belongs to an existing persistent task and can match an event source/type, a periodic heartbeat, or both. Waiting itself does not make a model request; a delivered wake can start a new turn and consume usage.

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

Due wakes for the same task are combined, and a busy task receives pending wakes at an idle boundary. Event ingress uses the existing app-server connection and bounded typed metadata, not a new public webhook endpoint. See the [event and heartbeat API example](codex-rs/app-server/README.md#example-subscribe-a-thread-to-events-and-heartbeats-experimental) for authenticated remote integrations.

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
