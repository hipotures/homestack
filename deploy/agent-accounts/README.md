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

## Local and public addresses

In the HomeStack deployment form, **Hostname** and **Domain** set the container's
local DNS address. Public HTTPS naming is configured separately:

- **Pangolin subdomain** is the public prefix, initially `agent-accounts` from
  the manifest; change it to `limits` for a different public name.
- **Pangolin domain** selects a base domain verified in Pangolin. With a prefix
  `limits` and base `apps.example.com`, the public URL is
  `https://limits.apps.example.com`.
- Review explicitly shows **Public URL** and the backend address before Apply.

The public name is saved per named deployment under the active HomeStack
profile's private `publications/` directory and restored when reopening the
form. A saved domain that is no longer verified is retained as an invalid
selection until the operator chooses a current domain; it is never silently
replaced by another available domain. Publishing a different name does not
delete an existing Pangolin resource or its DNS record.

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

A usage window has not started when it reports zero usage with no reset time or
a reset always one full window away; a reported reset that already passed has
ended. The shortest (session) window always shows a start control that opens a
dialog: start now, or set a daily auto-start. POST `profiles/<id>/window` sends
one minimal model turn so the provider starts counting its windows, then checks
usage. Claude uses the SDK helper with the Haiku alias, a one-line system
prompt, no tools, settings, hooks or MCP, and thinking disabled (about 1.3k
input tokens). Codex starts an ephemeral app-server thread with replaced base
instructions, the cheapest listed `*-luna` model, low effort, and tools, skills,
plugins, multi-agent features, project documents and MCP disabled (about 4.7k
input tokens, mostly cached). Platform sandbox and environment wrappers remain.

Claude's 7-day window can read 100% for a long time while requests still
succeed. While it reads 100%, every usage check also sends the same minimal
turn: a turn rejected for `seven_day` marks the window exhausted until that
reset without further turns; a completed turn marks it not exhausted. A failed
check leaves exhaustion unknown, and readers then fall back to 100% usage. The
5-hour window keeps the 100% rule.

Daily auto-start is PATCH `profiles/<id>` with
`{"auto_start":{"time":"08:00","tolerance_minutes":60,"timezone":"Europe/Warsaw"}}`
(`null` clears it). The time is local to the IANA time zone, so it follows
daylight saving changes. From that time until the tolerance (1–720 minutes)
ends, the scheduler starts the session window as soon as the latest usage
snapshot shows it is not running, unless a start request already completed
after that time. A failed start retries after two minutes.

REST resources are under `/api/v1`: `providers`, `profiles`, `runtimes`,
`operations`, and `health`. Profile subresources include `authentication`,
`limits`, `history` (latest 100 stored snapshots), `statistics` and `window`.
POST authentication/limits/statistics/window returns an operation ID. Unsupported
provider statistics return an explicit unsupported-capability operation error. Read
`operations/<id>` or subscribe to `operations/<id>/events` for progress.
Active operations are in-memory. Sanitized terminal summaries and runtime
operations are persisted in SQLite; authentication must be restarted after
service restart.

Mutation requests require `Content-Type: application/json`. Profile creation
accepts `provider` and `display_name`; PATCH accepts `enabled`, `last_payment`
and `auto_start`; DELETE requires
`{"confirm":true}`. No arbitrary RPC, command, file or terminal interface exists.
Claude code submission uses POST `profiles/<id>/authentication/code` with
`{"code":"..."}` only while that selected profile awaits input. The UI is
served by Rust from embedded static assets.

## Configuration

- `HOMESTACK_LISTEN`: default `0.0.0.0:8080`.
- `HOMESTACK_HEARTBEAT_SECONDS`: injected by HomeStack, default 60.
- `AGENT_ACCOUNTS_INTERVAL_SECONDS`: 60–3600, default 240.
- `AGENT_ACCOUNTS_PROBE_CONCURRENCY`: 1–2, default 1.
- `AGENT_ACCOUNTS_HOME`: absolute persistent mount, default `/home/app`.
- `AGENT_ACCOUNTS_BUNDLED_RUNTIMES`: image seed directory, default
  `/opt/agent-accounts`; used only for a provider's first installation.

Every account invocation uses the pinned absolute binary, an environment
allowlist, profile-specific HOME and provider configuration, file credential
storage for Codex and disabled provider update behavior. Inherited API keys, host login overrides, proxy
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

## Runtime upgrades

The image's initial Codex/Claude pins seed shared versioned installations only
when that provider has no persisted pin. Startup checks each pinned binary's
reported version. A missing or mismatched pin is a startup error; image refresh
never silently chooses the image's newer bundled version. A per-home file lock
prevents two service processes from sharing credentials and running probes.

The service checks official stable releases at startup and every hour, even
when no browser is open. **Provider runtimes** displays the latest discovered
version and highlights it when newer than the active pin. The open page refreshes
within its normal 15-second polling cycle. **Check available version** also
checks on demand. Checking versions installs nothing; select **Upgrade** to
activate a newer release. A busy provider skips that automatic check until the
next hourly cycle. Failed checks are logged and retried in the next cycle;
the last successful version and check timestamp remain visible.
An upgrade to a new version stages an official versioned musl download, validates its official
SHA-256 and reported version, and performs an account-limits probe using one
enabled authenticated profile. Only then does one SQLite transaction activate
the pin and store the real measurement. Candidate verification excludes other
operations for that provider and uses the same global process-slot limit.

Failure, cancellation, missing verification credentials or an incompatible SDK
keeps the previous working pin. The UI shows the blocker/rollback result.
The active and previous installation remain available; older installations
are retired only after successful activation. Interrupted staging is removed
at startup, without automatically resuming an upgrade. Interrupted operations
remain visible; inspect the active pin if the process stopped during completion.
A runtime upgrade never copies or replaces profile credentials.

When an older installation is retained, **Rollback** asks for confirmation
before selecting it. It verifies that the retained binary reports the expected
version and successfully probes real account limits before atomically changing
the pin. A failed check leaves the active version unchanged. Both installations
remain available after the switch. The newest known release remains visible,
and **Upgrade** can reactivate the retained newer installation after the same
verification, without downloading it again. Runtime rollback does not replace
the service image or revert account credentials, measurements or history.

REST operations:

- POST `runtimes/<provider>/check` with `{}` checks the official stable release.
- POST `runtimes/<provider>/upgrade` with `{"version":"X.Y.Z"}` explicitly
  selects a stable vendor release. The UI selects the discovered latest version.
- POST `runtimes/<provider>/rollback` with
  `{"version":"X.Y.Z","confirm":true}` selects the currently retained older
  installation. Missing confirmation or a changed retained version prevents
  activation. The upgrade endpoint refuses downgrades without this action.
- Both return operation IDs; GET/SSE progress uses the ordinary operations API.
- Candidate downloads are limited to 512 MiB, metadata to 2 MiB. The upgrade
  deadline is ten minutes; the real quota-probe deadline remains 60 seconds.

Claude invocations set both `DISABLE_AUTOUPDATER=1` and `DISABLE_UPDATES=1`.
Codex invocations set `check_for_update_on_startup=false`. Neither adapter
invokes a vendor updater or resolves binaries from a profile's PATH.
Antigravity invocations set `AGY_CLI_DISABLE_AUTO_UPDATE=true`; the Claude update
variables do not disable Antigravity's updater. This preserves the installed
binary at its recorded version between managed upgrades and process restarts.
The value must be the literal `true`; `1` still starts its background updater.
The SDK itself stays pinned to the service image and changes with a service
release; an incompatible CLI candidate cannot pass its usage verification.

## Telemetry and account health

Set the schema-3 **Collector metrics URL** input to the existing HomeStack
Collector's HTTP(S) `/v1/metrics` endpoint. It becomes
`OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`. No Collector, Prometheus or authentication
service is deployed by Agent Accounts. Native `runtime.telemetry: homestack`
declares the separate status endpoint; it does not supply an OTLP destination.
Existing deployments must provide this new manifest input before refresh.
For direct disposable runs the variable can be omitted to disable export.

Rust exports OTLP/HTTP JSON every 60 seconds, with a three-second export timeout
and bounded responses. Export failures are logged by category and the next
cycle continues. No credential values or account emails enter metrics. Labels
are provider, profile ID, account scope and limit window; runtime version is
used only on runtime metrics. The resource service name is `agent-accounts`.

Metrics include:

- `agent_account_limit_used_percent` and
  `agent_account_limit_reset_timestamp_seconds`, using actual observation times.
- `agent_account_enabled`, `agent_account_authenticated`,
  `agent_account_measurement_stale`, and
  `agent_account_last_success_timestamp_seconds`.
- `agent_account_probe_failures_total` and
  `agent_account_probe_duration_seconds`.
- `agent_account_alert` with bounded `condition` values for authentication
  expiry/reauthentication, repeated failures, unreachable provider and stale data.
- `agent_account_runtime_version_info`,
  `agent_account_runtime_verified_timestamp_seconds`,
  `agent_account_runtime_upgrade_failed`, and
  `agent_account_runtime_verification_failed`.

Use `agent_account_alert` for account alerts: disabled profiles explicitly emit
zero for these conditions. Raw authentication and stale gauges remain factual
when a profile is disabled; consumers deriving their own alerts must filter on
`agent_account_enabled`. Repeated failure means at least three consecutive
failed probes. Unknown quotas and missing success/reset timestamps are omitted,
not exported as zero. Last successful values retain their observation time;
failure or an elapsed reset marks them stale. Nothing infers fresh availability
from a percentage or timestamp. Readiness depends on local operations and
remains independent of provider/Collector availability.

## Adapter boundary

`Provider` implementations declare capabilities and return structured account
metadata and scoped snapshots. Unsupported operations have an explicit default
error; adapters need not implement authentication, limits, statistics or a
runtime they do not support. Optional bundled runtime metadata belongs to the
adapter. Add statically registered adapters without a dynamic plugin loader.
Vendor release/download mechanics are isolated in the small runtime module;
a new vendor's updater needs its own verified source implementation before
advertising the runtime-updates capability.

The scheduler, database, API, UI operations and metrics consume common models.
Authentication URL/code state is transient; only sanitized operation summaries
are persisted. Account statistics, subscription quota windows and any future
instance/session measurements stay separate. Remote discovery, distributed
App Servers and session monitoring are not implemented.

## Operational limitations

Pangolin protects the external administrative interface. Anyone with direct
management-network access to the internal HTTP listener can manage profiles;
restrict that network accordingly. No separate application login is required.
Profiles are isolated process/configuration environments within one trusted
service user, not independently sandboxed tenants. There is no generic shell,
file, PTY or App Server RPC endpoint, and probes never submit user tools/prompts.

The application supports only Linux amd64 images at present. Provider network
access uses the sanitized environment; inherited proxy credentials, API keys
and host-agent sessions are intentionally unavailable. Subscription quota
availability and identity fields depend on the provider/account permissions.
Claude's experimental SDK API and provider CLI login text may change. Failures
stay confined to the affected adapter and retain prior successful data.

History is retained until profile deletion; monitor persistent-volume space.
The UI shows the latest stored history, not an independently audited provider
billing record. Resource defaults were checked with startup, login initiation
and unauthenticated helper initialization. Authenticated-probe peaks and two
simultaneous authenticated Claude probes have not been measured. Raise memory
if the real workload requires it.

### Task 3 verification evidence

Host and static musl builds and focused Clippy with warnings denied passed.
The final production image was 786,377,053 bytes unpacked and reported about
7.1 MiB Rust-process RSS after the final checks. Its binary SHA-256 matched
the compiled artifact. All checks used disposable rootless Podman containers,
1 CPU, 1 GiB RAM and no swap; no production LXC or account files were accessed.

Both official stable-version checks succeeded: Codex 0.166.0 and Claude Code
2.1.318. Checking versions left the pins unchanged. Explicit upgrade requests
without an enabled authenticated account returned `verification_blocked` and
kept both pins. The pins, latest-version metadata and disabled profiles survived
container replacement with the same persistent volume. Another service process
using that home was refused by the instance lock.

The running Codex command used its persistent pinned path and explicitly
passed disabled startup update checks and file credential storage. The actual
Claude login child's executable, HOME and CLAUDE_CONFIG_DIR matched the selected
profile and pin, and both update-disable variables were 1. Cancelling that flow
and deleting its profile left no CLI/helper process.

A temporary instance of the existing HomeStack Collector 0.1.10 image accepted
and forwarded the application's real OTLP/HTTP JSON batches to a disposable
receiver. Disabled-profile alert points were all zero. Unauthenticated profiles
produced no invented quota-percentage metric. These checks did not deploy or
modify a production Collector or the `ai-usage` service.

With container networking disabled, an official release check failed with
`network_failure` while readiness remained `pass`. Desktop and mobile Chromium
checks verified runtime-version discovery, current-version upgrade disabling,
progress and error reporting, with no page errors or horizontal overflow.

Successful authenticated quota retrieval, activation of a newer runtime after a
real account probe, and rollback following a failed authenticated candidate
probe remain unverified without real accounts. The tested missing-account
blocker is not evidence of successful candidate installation/activation.
Claude's SDK initialization without credentials was verified separately in
Task 2. Proxmox LXC replacement and authenticated peak memory remain untested.

### Recovering an unqueued tag publication

If a pushed Agent Accounts tag did not start its release run, dispatch the
existing `ci.yml` workflow on the current default branch with
`agent_accounts_release_tag` set to that existing annotated tag. The checks and
image job both check out the tagged commit, verify the annotated tag resolves
to that exact commit, and preserve the immutable numeric image version and
source revision. Other service images are skipped for this recovery request.
Existing image admission still refuses overwriting any published release.
Leaving the input empty preserves the ordinary development-image workflow.

For Task 3, the SSH transport stopped authenticating. The already-configured
GitHub HTTPS credential synchronized the same upstream without changing Git
configuration. Tag `agent-accounts-0.1.2` was pushed normally, but GitHub did not
queue a tag run; the narrow dispatch recovery path was added to finish that
publication without moving the tag or choosing another version manually.


## Antigravity provider

Antigravity bundles the official 1.5.0 Linux x64 musl executable, verified against
its release archive SHA-256 digest. It is statically linked and adds approximately
208 MiB of unpacked runtime data. Runtime updates use the official GitHub release
metadata and archive digest, with the same candidate verification and activation
workflow as the other providers.

Google subscription sign-in runs `antigravity acp --login` through util-linux
`script`, with terminal echo disabled. The UI displays the Google OAuth PKCE URL
and accepts the authorization code or final redirect URL. Each profile has its own
HOME and file-based credential storage; no desktop keyring or D-Bus is configured.
Credentials stay under the persistent profile home. Successful sign-in still
requires the account owner to complete Google's browser authorization.

The provider reads the subscription plan from ACP `session/new` response metadata
`_meta.antigravity.tier.name`. The account email is currently unavailable through
this integration and is shown as unidentified. Quotas come from the structured
`command.data.groups` in `-p /usage --output-format json`, with remaining fractions
converted to used percentages. All groups are stored; the card selects one group
at a time and defaults to Gemini 4 Argon when present. The selection does not
change the CLI model or consume model inference tokens.

The default rootfs is 2 GiB to leave space for the additional bundled runtime.

Local verification used the production Containerfile in a rootless Podman OCI
container limited to one CPU, 1 GiB RAM and no swap, with disposable storage
explicitly enabled. The unpacked image measured 1,008,823,211 bytes. API profile
creation, Google PKCE URL and code prompt delivery, and cancellation passed;
cancellation left only the service and init processes. Observed memory was about
75 MB while waiting for authorization and 28 MB after cancellation. This is not
Proxmox LXC evidence. Browser consent, persisted authenticated restart, full
account quota retrieval in Alpine and visual browser assessment remain unverified.

AI Usage quota overlays consume `agent_account_limit_window_minutes` and
`agent_account_limit_exhausted` alongside the existing utilization and reset gauges.
All four carry the same provider/profile/window labels plus `window_label` and
`model`. Exhaustion is the provider flag when supplied, otherwise utilization at
least 100%; false is exported explicitly to clear previous observations.

## Telegram weekly reset notifications

Create `$XDG_CONFIG_HOME/homestack/telegram.toml` (normally
`~/.config/homestack/telegram.toml`) on the desktop running HomeStack:

```toml
[telegram.notifications]
enabled = true
bot_token = "YOUR_BOT_TOKEN"
chat_id = "-100YOUR_CHAT_ID"
```

Keep the file local, preferably with mode `0600`. HomeStack reads it during
service deployment, configuration review and image updates. It supplies
`AGENT_ACCOUNTS_TELEGRAM_ENABLED`, `AGENT_ACCOUNTS_TELEGRAM_BOT_TOKEN` and
`AGENT_ACCOUNTS_TELEGRAM_CHAT_ID` through the existing application environment
mechanism over the Proxmox API. No SSH or Pangolin configuration is involved.
The owner explicitly permits the Telegram bot token in the container environment,
where Proxmox administrators can read it. Values are not shown in deployment
reviews or copied into saved application selections. No Telegram inputs appear
in the deployment form. Updating the desktop file takes effect after applying
service configuration or replacing the image; editing it does not change an
already running container automatically.

A missing file or `enabled = false` disables notifications. When enabled,
`bot_token` and `chat_id` are required. For direct runs, supply the same three
environment variables. The bot must already have permission to send to the
specified chat. These settings are not inherited by provider CLI processes.

Only weekly quota windows (10080 minutes) of enabled profiles trigger quota-reset messages,
for all supported providers. A first observation establishes the baseline;
a 5-hour reset alone sends nothing. A scheduled reset is confirmed by a fresh
observation after the previous deadline with a new future reset deadline.
An early reset is inferred when used quota drops by at least five percentage
points to at most 5% used, including a first post-reset reading of 98% available.
This inference cannot distinguish a vendor correction from a reset, or detect
a reset hidden by substantial usage between observations. No message is sent
merely because the local clock passes a deadline. Changed account identities
establish a new baseline instead of being reported as resets.

One English message groups weekly windows reset in the same observation and
reports the provider, profile name, available percentages, observation time and
next reset deadlines in UTC. The snapshot and pending message are committed
together in the existing profile record, including successful runtime
upgrade/rollback probes. Pending messages survive process and image replacement.
A separate worker delivers them without blocking usage checks. Failed network
requests and server errors retry with backoff; Telegram rate limits honor
`retry_after`. Rejected requests are logged without credentials and removed
from the queue. A successful send removes its pending message. A lost response
or interruption between Telegram acceptance and local acknowledgement can cause
a duplicate on retry; Telegram sendMessage has no idempotency key. Telegram
outages do not change service readiness.

An image update accepts an already pending `env` change only when every changed
variable is supplied by the application configuration selected for that update
and no existing variable is removed. These pending values are included in the
reviewed state fingerprint. Before replacement, the selected application values
replace any older pending values, then take effect during the normal stop/start.
Other pending configuration changes still require review; the error identifies
the field without printing its value. A separate restart is not required for
matching application settings.

### Startup verification and header indicator

On each service-process start, enabled Telegram notifications send one message:
"Agent Accounts started. Weekly quota reset notifications are enabled."
Only Telegram's successful `sendMessage` acknowledgement marks the configuration
verified. Failed startup messages retry with backoff (five minutes for rejected
credentials/chat access, and honoring `retry_after` for rate limits); the service
and its health endpoints remain available. Queued quota-reset notifications wait
until the startup message succeeds. Missing or malformed optional Telegram
configuration disables notifications and logs a sanitized error without stopping
the service; correct its environment and restart to retry that configuration.
A lost acknowledgement can duplicate a startup message on retry, as with
ordinary notifications.

The header shows the Telegram logo in one of three colors: blue after a
successful delivery, gray when notifications are not configured, and red when
the optional configuration is invalid or the last delivery attempt failed. The
logo stays hidden until the first startup message completes and while the
browser has lost its service connection. `GET /api/v1/notifications/telegram`
returns only `{"status":"pending"|"verified"|"disabled"|"failed"}`; credentials
and chat identifiers are never returned. This indicator reflects the last
completed delivery attempt, not a continuous connectivity probe. The logo is
Telegram's official mark from https://telegram.org/tour/screenshots, inlined in
the page without external requests.
