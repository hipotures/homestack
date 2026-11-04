# Agent Accounts

Agent Accounts manages isolated subscription profiles. It is independent of
`ai-usage`; it neither reads nor changes that service's Prometheus data.

## Deployment contract

Deploy the schema-3 `agent-accounts` service from this repository in HomeStack.
Choose the published image version, persistent storage and Pangolin domain.
Pangolin provides external HTTPS and authentication. Internal HTTP uses port
8080. Keep direct access restricted to the management network.

The image uses Alpine 3.24.2, official Codex 0.166.0 and Claude Code 2.1.318
musl binaries verified against release SHA-256 values, and Claude Agent SDK
0.3.318. Node.js runs only the short-lived usage helper. Bash, libgcc,
libstdc++ and ripgrep cover the documented Claude musl dependencies.
Tini reaps subprocess descendants. The
Rust service runs as UID/GID 10001. HomeStack must preserve ownership of the
persistent mount. Nothing baked into `/home/app` is required for initialization.

Readiness is `/health/ready`; `/health/live` reports process heartbeat health.
`/.well-known/homestack/status` implements native status schema 3. The heartbeat
passes through the HTTP listener and performs a SQLite read. Provider outages
are profile errors, not readiness failures.

The initial resource defaults are one CPU, 1 GiB RAM, zero swap, 1 GiB disposable
root and 4 GiB persistent home. The workload is short-lived authentication and
quota reads, with one provider operation at a time by default, at most two.
These defaults are not a claim of measured production capacity.

## Persistent storage

All state lives below `AGENT_ACCOUNTS_HOME` (default `/home/app`):

- `database/accounts.db`, including adjacent SQLite WAL/SHM files: metadata,
  pins, measurements and history. Schema updates are transactional.
- `profiles/<uuid>/home`, `codex`, `claude`, `work`, `tmp`: isolated profile
  state. Credentials stay in provider-managed files and never in the database.
- `runtimes/<provider>/<version>/<provider>`: shared, versioned installations.
- `state`: service-owned operational state.

The service refuses a symlinked or unmounted home. For disposable local checks,
set `AGENT_ACCOUNTS_EPHEMERAL=1` and choose a repository-local temporary home.
Never enable that option for normal deployment. Use SQLite's Online Backup API
or `.backup` for database snapshots; never copy a database with `cp`.

Disabling preserves credentials and history and stops scheduled checks.
Deletion requires explicit confirmation and permanently removes the selected
profile directory, database record and all local history. Interrupted deletion
resumes at startup. Other profiles remain independent.

## Providers and API

Supported providers are Codex and Claude Code. Device authentication uses the official App
Server protocol. Open the URL and enter the device code shown by the UI;
no pasted authorization code is required. The provider owns token persistence
and renewal. API-key accounts are not subscription-monitoring profiles.

Codex account quota reads use `account/rateLimits/read`, without a conversation.
Optional statistics use `account/usage/read` without a thread ID: in the pinned
CLI this reads backend account activity. Local/session statistics are not
reconstructed from this monitoring process. Missing values remain unknown;
failed probes preserve the previous successful snapshot as stale. A reset time
passing never implies zero usage or restored availability.

REST resources are under `/api/v1`: `providers`, `profiles`, `runtimes`,
`operations`, and `health`. Profile subresources include `authentication`,
`limits`, `history` (latest 100 stored snapshots), and `statistics`.
POST authentication/limits/statistics returns an operation ID. Unsupported
provider statistics return an explicit unsupported-capability operation error. Read
`operations/<id>` or subscribe to `operations/<id>/events` for progress.
Operations are in-memory with sanitized completed summaries in SQLite;
authentication must be restarted after service restart.

Mutation requests require `Content-Type: application/json`. Profile creation
accepts `provider` and `display_name`; PATCH accepts `enabled`; DELETE requires
`{"confirm":true}`. No arbitrary RPC, command, file or terminal interface exists.
Claude code submission uses POST `profiles/<id>/authentication/code` with
`{"code":"..."}` only while that selected profile awaits input. The UI is
served by Rust from embedded static assets.

## Configuration

- `HOMESTACK_LISTEN`: default `0.0.0.0:8080`.
- `HOMESTACK_HEARTBEAT_SECONDS`: injected by HomeStack, default 60.
- `AGENT_ACCOUNTS_INTERVAL_SECONDS`: 60–3600, default 300.
- `AGENT_ACCOUNTS_PROBE_CONCURRENCY`: 1–2, default 1.
- `AGENT_ACCOUNTS_HOME`: absolute persistent mount, default `/home/app`.
- `AGENT_ACCOUNTS_BUNDLED_RUNTIMES`: image seed directory, default
  `/opt/agent-accounts`; used only for a provider's first installation.

Every invocation uses the pinned absolute binary, an environment allowlist,
profile-specific HOME and CODEX_HOME, file credential storage and disabled
Codex startup update checks. Inherited API keys, host login overrides, proxy
credentials, session variables and telemetry settings are not forwarded.

## Releases and verification

Build with `python3 scripts/cargo.py build --locked -p hs-agent-accounts
--bin homestack-agent-accounts`. CI builds a static musl release binary,
validates the image locally and publishes only after existing release checks.
After committing and pushing, run
`python3 scripts/tag-image.py --service agent-accounts`. Never choose or move
release tags manually. Publishing an image does not replace a running LXC.

### Task 1 verification evidence

Local production-image verification used rootless Podman with one CPU, 1 GiB
RAM, no swap, and a separate named volume. The unpacked image was 340,027,769
bytes. The service reported approximately 6.5 MiB RSS after cold startup.
Readiness, heartbeat and native status passed; startup without a mounted home
was refused. Creating and disabling a profile, replacing the container root
while retaining the volume, and deleting the restored profile all passed.

Host API checks also verified unauthenticated-probe errors with unknown usage,
profile lifecycle and device-login URL/code delivery. A Chromium check at
390×844 verified profile creation and the real device authorization dialog,
with no JavaScript errors or horizontal overflow. Login was cancelled and
the disposable profile removed. No actual account authentication or successful
quota reading was performed; credentials were not imported. These checks are
OCI evidence, not a Proxmox LXC replacement test. Probe memory under an
authenticated workload remains unmeasured.

## Claude adapter and experimental dependency

Claude subscription login uses the official `claude auth login --claudeai`
command. Version 2.1.318 supports ordinary stdin/stdout, including URL display
and pasted authorization codes; no PTY or general terminal endpoint is needed.
The selected profile supplies HOME, CLAUDE_CONFIG_DIR and XDG directories.
`claude auth status --json` must report that exact configuration directory.
Cloud/API-key environment overrides and parent Claude session variables are
not inherited. Both DISABLE_AUTOUPDATER and DISABLE_UPDATES are set to 1.

The short-lived ESM helper is packaged with a dependency lockfile. Its stdin
request is `{version:1,id,binary}`; its stdout reply has the same version/ID,
`ok`, and either account-limit data or a categorized error. Only Rust selects
the binary, launches the process, enforces deadlines and stores observations.
The helper has a 45-second abort timer within Rust's 60-second probe budget.

The SDK `query()` input is an async iterator that never yields a user message.
After initialization, it invokes
`usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({skipBehaviors:true})`
and closes the query. It disables tools, hooks, project/local settings,
filesystem/account MCP connections, IDE integration and session persistence.
This control method is experimental and isolated behind the Claude adapter;
an incompatible change becomes a provider failure, not a service crash.

SDK 0.3.318 plan utilization is already 0–100. Five-hour, seven-day,
OAuth-app, Opus/Sonnet and model-scoped windows are retained when supplied.
Optional extra spending uses a valid used/limit ratio or the supplied percent.
Session totals and local transcript analysis are deliberately not collected.
Missing fields are unknown. No active conversation is required.

Provider references:

- https://learn.chatgpt.com/docs/app-server
- https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/app-server-protocol/src/protocol/v2/account.rs
- https://code.claude.com/docs/en/authentication
- https://code.claude.com/docs/en/setup
- https://github.com/pingdotgg/t3code/blob/main/apps/server/src/provider/ClaudeProvider.ts
- https://github.com/pingdotgg/t3code/blob/main/apps/server/src/provider/claudeUsageLimits.ts
- The installed `@anthropic-ai/claude-agent-sdk` 0.3.318 `sdk.d.ts` is the
  authoritative signature/percentage reference for this helper.

### Task 2 verification evidence

The production image built successfully and was 785,872,222 bytes unpacked.
It ran under rootless Podman with the declared 1 CPU / 1 GiB / no-swap limits
and the Task 1 persistent volume. Codex's pin survived and Claude's pin was
seeded independently. Two Claude profiles and one Codex profile coexisted.
Both Claude `auth status` calls reported their selected configuration directory;
neither was logged in. Claude login produced a URL and pasted-code prompt,
and cancellation left only Tini and the Rust service running.

Invoked directly with an isolated unauthenticated profile, the packaged SDK
helper returned protocol version 1 and its request ID, exited successfully in
0.87 seconds, and reported `rate_limits_available=false`. No prompt was
submitted, no unavailable quota was replaced with zero, and no helper/CLI
process remained. This proves initialization and termination without a login,
not authenticated subscription retrieval or token-accounting behavior.

A Chromium mobile check verified both-provider cards, the Claude URL/code
form and cancellation, with no page errors or horizontal overflow. All disabled
profiles survived service restart and were then deleted through the API.
Authenticated-account coexistence, account identity confirmation and genuine
Claude quota values remain unverified because no credentials were imported.
