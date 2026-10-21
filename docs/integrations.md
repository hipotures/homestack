# Desktop integrations

HomeStack stores integration files under `$XDG_CONFIG_HOME/homestack/integrations`
(default: `~/.config/homestack/integrations`):

- `ovh-dns`: OVH application key, application secret, consumer key and endpoint.
- `spaceship-dns`: Spaceship API key and API secret.
- `dns-tsig.key`: the BIND TSIG key used for local DNS updates.
- `settings.toml`: each provider's test zone, local DNS server and last test report.

Credential files and integration settings are written atomically with mode `0600`.
Reports contain the UTC completion time, tested zone and endpoint, outcome,
duration and details with credentials redacted. Test targets are remembered when
starting a test or leaving its form. A successful test automatically saves the
credentials it verified only when there are pending changes and the form has
not changed while the test was running. Testing unchanged credentials updates
only the report and does not rewrite the credential file. Failed or stale tests
never save credentials. F5 saves pending changes manually and is disabled when the form matches the saved configuration. Save
failures appear in the report and leave pending changes available to retry.

Settings > Integrations groups OVH, Spaceship and Local DNS under DNS. Pangolin
appears under Remote Access and opens the setup wizard described below.

The installer uses the DNS provider detected for the server domain and writes
the selected credential path and public DNS zone into `deploy.toml`. E2E uses
the Spaceship and TSIG files in the same directory by default; explicit
`--spaceship-file` and `--tsig-file` paths still select the requested files.
`homestack-deploy` uses the credential paths explicitly saved in its deployment
configuration. `tui.toml` contains interface preferences, not integration data.

## Pangolin setup wizard: Steps 1–6

Open **Settings > Integrations > Remote Access > Pangolin** to enter the setup
wizard. All eight stages are shown: Pangolin server, Pangolin prerequisites,
Pangolin discovery, Integration API, API tunnel, Bootstrap API key, HomeStack API
key, and Integration verification. The first six stages are implemented;
stages 7–8 remain locked and cannot execute operations.

Steps 1–6 share the same focus navigation. Enter or Right from the step list
focuses the selected step's controls. Up/Down selects steps while the list has
focus and cycles controls while the details have focus, like Tab/Shift+Tab.
Esc from the details returns focus to the step list without discarding inputs,
clearing the bootstrap credential or disconnecting the tunnel. Esc from the
step list leaves the wizard, with confirmation for unsaved server edits.
Closing a modal or cancelling an operation takes priority over moving focus;
press Esc again afterward to return to the list. The navigation highlight and
footer labels follow the active pane. Completing a test keeps the current step
and focus; Esc from the Test button returns to the step list.

On wide terminals, the step list sits to the left of the selected step's details.
At 80x24, all eight steps appear above a compact form. Up/Down in the step list
selects any stage, including locked stages for explanation. Enter or Tab on the
first stage moves to the **SSH connection alias / server hostname** field;
Tab/BackTab move between navigation, field and Test. F4 opens a scrollable
contextual help panel for the selected stage; Esc closes it without leaving the
wizard. Help is available for all eight stages. Unimplemented stages have no execution
controls. The field accepts
an OpenSSH alias, hostname, IP address or `user@host`; it never assumes `root`.
OpenSSH resolves the user's existing configuration, including user, port,
identities, proxy settings and hardware authentication. There are no embedded
server addresses. Only a successful test saves the target; there is no manual
Save action or F5 shortcut. F6 tests the current draft; F7 opens the
last report. Esc cancels a running test, then returns with an additional discard
confirmation when unsaved changes exist. Ctrl+U clears the selected field.

The asynchronous test resolves effective configuration with `ssh -G`, then
establishes a fresh authenticated SSH connection and executes only `true`.
It uses the existing HomeStack askpass UI for host-key confirmation, PIN,
passphrase and hardware-key touch prompts. The existing `touch::touch_wait`
mechanism uses `[sounds]` settings for waiting, success and failure cues (including
the existing terminal-bell fallback). Instant success stays silent as in other
HomeStack SSH flows. Host verification remains enabled;
configurations disabling it are refused. Password and keyboard-interactive
fallbacks remain disabled, consistent with the existing HomeStack SSH flow.
Forwarding, LocalCommand, configured RemoteCommand, connection sharing and
background execution are disabled for this test. The total deadline is 45
seconds (including authentication); connection establishment has a 15-second
timeout. Editing or closing the form cancels its worker. Concurrent tests are
blocked until the previous worker finishes. Raw SSH output is never displayed,
logged or persisted; diagnostics use fixed failure categories.

The selected target is stored atomically with mode `0600` in
`integrations/pangolin.toml` as `server_target`. New directories are private
(`0700`). No SSH passwords, PINs, private keys or API keys are stored there.
Unrelated settings are preserved. Successful verification saves the exact
unchanged tested target; failure, cancellation and stale results do not save.
The wizard distinguishes Not configured, Saved/not verified, Testing, Verified,
Failed and Verification outdated. Editing invalidates verification even if the
old text is restored. A successful test atomically saves its exact target together
with the UTC test date, PASS outcome and duration. The screen shows the local
date as `DD.MM.YYYY HH:MM:SS`, with fractional seconds omitted, and test duration
in seconds for runs lasting at least one second. Reopening restores that recorded
verification and the 1/8 progress; it does not open an SSH session automatically.
A target without a matching successful report is saved but not verified. A verified target shows **1/8** and enables manual prerequisites.

Steps 1–2 do not inspect Pangolin, Docker or administrative permissions,
configure an API or tunnel, bootstrap credentials, or create remote objects.
It changes no VPS configuration. A successful SSH test does not establish that
Pangolin is installed or that any later integration stage works.

### Manual prerequisites (Step 2)

After Step 1 verifies and saves the current server, select **Pangolin
prerequisites**. Expand **Pangolin installation** and **Bootstrap API key** with
Enter or Right; Left collapses a header. Tab/Shift+Tab cycles navigation, the two
headers and the single confirmation checkbox. Space or a click toggles the
checkbox. Enter on the checkbox confirms and saves it if necessary, then selects
Pangolin discovery without executing it. A failed save keeps the current step. F4 opens scrollable
contextual help for every stage, including locked stages. PageUp/PageDown scroll
instructions while headers and confirmation stay visible. At 80x24, focusing a
control opens a compact details page; Tab back to navigation shows all eight
stages again. SSH Test and Save controls are absent from Step 2.

Pangolin Server must already run on the selected server. No installation method
is assumed; the Integration API may remain disabled. In Pangolin's administration
interface, create a temporary API key named `homestack-bootstrap` with **Root**
authorization and these four actions:

- `createApiKey`
- `listApiKeys`
- `listApiKeyActions`
- `setApiKeyActions`

Also enable the recommended optional **`getOrg` (Get organization)** action.
It lets Step 6 confirm the Organization ID immediately. Without it, bootstrap
key validation can still succeed, but organization existence remains unconfirmed.

Root authorization is required for Integration API key management; selecting the
four actions alone is insufficient. Keep the token securely until masked entry in
Step 6. Step 2 has no token input and reads or writes no credential file. The
temporary administrative key is separate from the restricted HomeStack key;
revoke it only after permanent-key provisioning and verification succeed.

The checkbox confirms both instruction groups. It becomes checked and progress
changes from **1/8** to **2/8** only after an
atomic private write to the existing `integrations/pangolin.toml`. Unchecking
persists the incomplete state. Confirmation is associated with the exact verified
server target; editing the target invalidates it, and saving a changed target
removes the old confirmation. Unrelated settings are preserved. Write failures
leave confirmation unchanged and display an error; Space retries. Confirmation
restores on restart, but never claims technical verification of Pangolin or its
API.

### Installation discovery (Step 3)

Step 3 unlocks after the current saved server is verified and its prerequisites
are confirmed. F6 or **Discover** starts a bounded, asynchronous, read-only SSH
inspection. Tab/Shift+Tab cycles navigation, Discover and (when available) Report;
Enter activates the selected action. F7 opens the scrollable inventory, F4 opens
contextual help, and Esc cancels a running discovery. Mouse actions use the same
controls. At 80x24, Tab into the details opens a compact page; Tab back to
navigation shows all eight steps. Later stages have their own dependency checks.

The existing SSH/askpass and hardware-touch mechanisms are reused. Discovery
uses a fresh authenticated connection because the existing bounded SSH worker
explicitly disables connection sharing. Host verification and authentication
settings are unchanged. The overall deadline remains 45 seconds, including
configuration resolution and authentication; each remote subprocess has a
6-second timeout. Python 3 must already exist on the remote server; nothing is
installed and bytecode writes are disabled.

The fixed remote Python probe performs only these operations:

- `docker ps -a --no-trunc --format ...`: identify official `fosrl/pangolin`
  image references (Docker Hub or GHCR), including stopped containers.
- `docker inspect --format ... <validated container ID>`: select running state,
  image reference, mounts, port bindings, PID and the two Compose labels. No
  environment or unrelated labels are requested.
- `systemctl list-unit-files --type=service` and narrowly selected
  `systemctl show`: identify Pangolin-named units and executable, active state,
  service-file and working-directory metadata. No service is started or changed.
- Read a bounded `config.yml`/`config.yaml` discovered from actual config mounts,
  extracting only literal `flags.enable_integration_api` and server port scalars.
- Read `/proc/<inspected PID>/net/tcp{,6}` for listening TCP ports. No `docker exec`,
  `nsenter`, `sudo`, API request, recursive filesystem search or configuration
  mutation occurs.

Docker Compose is distinguished by its runtime project labels. Plain Docker is
supported without Compose metadata. Native/systemd executable identities are
reported as partial inventory: arbitrary native configuration layouts are not
assumed. Multiple installations or unrecognized Pangolin-named service
executables are unsupported and never selected automatically. Custom image
names and non-systemd native deployments cannot currently be reliably identified.

Only allowlisted inventory leaves the remote probe. Raw configuration, process
and container environments, unrelated labels, SSH stderr and exceptions are
never displayed or persisted. Free-form remote diagnostics are discarded in
favor of fixed issue codes. Paths remain data and are never interpolated into a
shell command. The complete probe is embedded in the Rust binary and is auditable
at `crates/hs-cli/src/tui/pangolin_discover.py`.

Results distinguish Not discovered, Discovering, Discovered, Partially discovered,
Not found, Permission denied, Unsupported installation, Failed, Cancelled and
Outdated. A stopped installation is partial and explicitly marked Stopped.
Completion requires an identified running container and readable unambiguous
configuration. A disabled Integration API is acceptable; absent port settings
remain Unknown rather than assuming version-dependent defaults. An enabled API
also requires an explicit valid port and observable listening state. Bindings
are classified as loopback only, wider interface or not published. Socket access,
configuration permissions and unsupported YAML are explicit unresolved fields.

Image references are evidence of the configured image, **not confirmation of the
running software version**. Arbitrary YAML constructs (anchors, inline mappings,
expressions) and runtime overrides are not evaluated; unresolved values remain
unknown. Configuration is an observation of the file, not proof it matches the
already-running process. Inspect the F7 inventory before any future configuration
phase. Installation and application/API settings are observed independently.

Sanitized inventory, target, date, outcome and fixed diagnostic summary are saved
atomically in `[discovery]` in the existing private integration file. Repeated
runs replace that record. Restarting restores historical results, not continuous
monitoring. Editing the target invalidates discovery; failure never erases the
SSH target, prerequisite confirmation or unrelated settings. A failed save does
not count as completion. Successful discovery advances progress to **3/8**.

Discovery itself never enables API access, changes ports or restarts services.
Step 4 provides the separate, explicitly approved configuration workflow below.
No API keys or Pangolin objects are created.

Configuration keys were checked against the
[Pangolin v1.24.0 configuration schema](https://github.com/fosrl/pangolin/blob/1.24.0/server/lib/readConfigFile.ts).


### Step 4: private Integration API

Step 4 unlocks after successful discovery for the same verified SSH destination.
**F6 / Inspect API** runs asynchronously through the existing SSH and askpass
transport. It rechecks installation identity, configuration path, literal API
settings, service state and Docker port bindings. A bounded unauthenticated
`GET /v1/` must return HTTP 200 with `{"message":"Healthy"}` through an IPv4
loopback publication correlated with the Pangolin container. This request uses
no credentials, redirects or HTTP proxy environment variables. TCP listening
alone cannot pass. A healthy private API completes **4/8** without writes,
backups or restart. Repeat inspection remains read-only.

Use **Tab / Shift+Tab** to move between navigation, Inspect, Details and Apply;
**Enter** activates the focused control. **F4** opens scrollable help, **F7**
opens the scrollable plan/report, and **F5** opens Apply confirmation only for a
current applicable plan. Mouse actions use the same controls. Confirmation
starts on **Cancel**; Tab or Left/Right selects **Apply**, then Enter confirms.
Esc closes a modal, cancels an operation or returns. At 80x24, Tab enters the
full-height details page; navigation remains available by cycling focus.

When configuration is needed, the proposed plan identifies current/proposed
settings, exact files, host/container API ports, Compose project context,
service, interruption, protected backup location and verification steps.
An existing external API publication is explicitly identified: converting it
to loopback requires approval because other integrations may lose access.
Only confirmed Apply can modify the server. No OS accounts, permissions,
Docker groups, sudo, password fallback, proxies, firewall or DNS are changed.

Automatic editing is deliberately narrow: official Docker Pangolin discovered
in Step 3, a single Compose file, literal YAML, verified directory bind mount,
matching running Compose configuration hash and image, and ownership/write
access allowing atomic replacement. Remote Python 3, existing PyYAML and
Docker Compose v2 with JSON/config-hash output are required; HomeStack installs
nothing. Existing explicit API ports are retained. If a disabled installation
omits the API port, the plan explicitly proposes the documented default 3003;
it is never labelled observed runtime evidence. Short-form literal TCP port
lists support adding an IPv4 loopback mapping or converting a single external
IPv4 mapping while preserving its host port. Existing unrelated content and
settings are retained. The full effective Compose configuration is validated
with the actual project name/directory and explicit Compose file; environment
file interpolation is not introduced.

After confirmation, runtime, effective Compose model and file hashes/identities
are revalidated. Only changed files are copied into a collision-safe timestamped
remote directory with mode 0700; originals and recovery metadata have mode 0600.
No configuration contents or backups leave the VPS. Files are atomically
replaced with their original ownership and permissions. Only the Pangolin
service is force-recreated with `--no-deps --no-build --pull never`, preserving
its verified Compose settings. This can interrupt Pangolin, connected Sites,
Resources and active sessions. No images are pulled or other services restarted.
Private publication, runtime and API health are checked after activation;
sanitized discovery facts are refreshed for the recreated container.

Failures distinguish authentication, permissions, unsupported layout/YAML,
stale plan, backup, validation, activation and health problems. The report
states whether nothing changed, activation remains unverified, or the remote
outcome is uncertain. Backups remain available under the reported protected
path; `recovery.json` records original paths, hashes and ownership. HomeStack
does not retry mutations or roll back automatically. Inspect current files
and service before independently approving any recovery. Cancelling or losing
SSH during Apply does not guarantee that the remote operation stopped: inspect
before retrying. Inspection uses a 180-second overall SSH deadline; individual
remote commands and health requests are also bounded.

Only sanitized non-secret metadata is saved atomically in `[integration_api]`:
target, installation association, date, outcome, configured port, endpoint,
applied summary and backup reference. Plans remain in memory and require a new
inspection after restart. Historical verification is not continuous monitoring.
Server changes or new discovery invalidate Phase 4; unrelated credentials and
settings remain intact. A persistence failure cannot complete the step.

Unsupported automatic changes include multiple Compose files, interpolation,
anchors/tags/duplicate YAML keys, env files, hooks, build/profile/extended
services, host/shared networking, ambiguous or single-file mounts, and native
service or plain-Docker reconfiguration. Already healthy ordinary Docker
installations can be verified without Compose editing. Custom reverse proxies
and firewall reachability are not audited: verification establishes the
container's loopback-only API publication, not a network-wide exposure audit.
Unrecognized layouts require independent configuration followed by inspection.
**Step 4 does not open a tunnel or request an API key.** Step 5 below manages the private transport separately.

The unauthenticated health contract is checked against the
[Pangolin v1.24.0 Integration API router](https://github.com/fosrl/pangolin/blob/1.24.0/server/routers/integration.ts).


### Step 5: managed private API tunnel

Step 5 unlocks only for the current verified SSH target, confirmed prerequisites,
successful discovery and verified private API endpoint. The host publication
port comes from the exact Phase 4 endpoint, which may differ from the container
API port. Missing, stale, non-loopback, credential-bearing or ambiguous endpoint
metadata is rejected; no default port or public fallback is used.

**F6 / Connect** starts asynchronously with the existing OpenSSH configuration,
askpass UI and hardware-key sound/touch mechanism. Host-key verification remains
enabled. Password and keyboard-interactive fallback, agent/X11/tunnel-device
forwarding, configured local/remote commands and background persistence are
disabled. No remote command, software installation, SSH configuration edit,
server change or API authentication occurs.

The wizard owns a foreground SSH `Child` and a unique control socket under a
private mode-0700 temporary directory. Initial `ClearAllForwardings=yes` removes
all forwards inherited from SSH configuration. After authenticated master
startup, a separate `ssh -F /dev/null -S <owned socket> -O forward` request adds
only `-L 127.0.0.1:<allocated port>:127.0.0.1:<verified host port>`, with
`ClearAllForwardings=no`. This avoids accidentally suppressing HomeStack's own
forward. The control request cannot fall back to a new SSH session or touch an
unrelated master. `GatewayPorts=no` preserves the explicit IPv4 loopback bind.

A temporary IPv4-loopback listener allocates a free unprivileged local port,
then releases it immediately before OpenSSH binds it. OpenSSH's successful mux
acknowledgement is required before probing HTTP. Bind races are handled with
up to three bounded retries through the same authenticated connection; unrelated
listeners are never stopped. Local ephemeral ports are not reused across runs.

Connected requires a direct bounded HTTP request through that newly established
forward: `/v1/`, HTTP 200, and exactly the Pangolin `Healthy` JSON object. Redirects,
proxy environment variables and credentials are never used. Responses are bounded
to 8 KiB of headers and 1 KiB of decoded body; HTTP/1.0 and HTTP/1.1 with content
length, connection-close or bounded chunked bodies are supported. The owned SSH
child must still be alive after health verification. Startup is bounded to 45
seconds and health requests to 2 seconds; cancellation is checked throughout.

The manager retains the process independently of the startup operation. It
survives navigation between wizard stages. **F6 / Verify** checks the existing
connection without reallocating its port or authenticating again. The process
is monitored for unexpected termination, including OpenSSH server-alive checks;
API health is refreshed explicitly with F6 rather than continuously polled.
SSH authentication, host verification, port conflicts, startup, remote API,
unexpected response, timeout and cancellation have separate sanitized messages.
Raw stderr, HTTP bodies and authentication material are not displayed or saved.

**F5 / Disconnect** terminates and reaps only this owned SSH process and its
private process group. The same cleanup runs on startup cancellation, wizard
exit, normal application exit, or invalidated server/discovery/API metadata.
Cleanup waits for its worker and owned child; no PID-file ownership or shared
ControlMaster termination is used. Abnormal process/worker exit invalidates the
live endpoint. Normal teardown does not leave an orphaned SSH connector.

Keyboard: Up/Down select stages in navigation; Tab/Shift+Tab move through the
available Connect/Verify and Disconnect controls; Enter activates them. F4 opens
scrollable contextual help and F7 a sanitized connection report. Esc closes help,
cancels a connection/verification, or leaves the wizard. Merely selecting another
stage or closing help preserves a connected tunnel. Cancelling an explicit
verification closes that connection; leaving the wizard always releases it.
Mouse actions match the keyboard. At 80x24, Tab opens full-height details and the
frame uses a compact F5 Stop label so Esc remains visible.

States are Disconnected, Connecting, Verifying, Connected, Connection lost,
Failed, Cancelled and Outdated. Progress is **5/8 only while connected** and
returns to **4/8** on disconnect. Only target, remote endpoint and last successful
health timestamp are persisted atomically under `[tunnel_history]`. Historical
success restores as **Disconnected**, never opens SSH on application startup,
and cannot serve as a live endpoint. No local port, PID, private key or API token
is persisted.

API consumers obtain the current endpoint from the owned runtime
`Session::endpoint(&current_spec)`. This validates the current server/API
association, verified session state and actual child liveness; it returns no
endpoint while verifying, after failure, or for a changed configuration. No new
tunnel is needed per API operation. Step 6 uses this live session; **Steps 7–8 remain unimplemented.**

The implementation requires Linux/Unix process groups, OpenSSH multiplexed
forwarding and the existing `setsid`/askpass infrastructure. A short local-port
allocation race remains inherent to OpenSSH TCP local forwarding; explicit mux
acknowledgement and bounded retries handle it. There is no automatic reconnect
or promise of child cleanup after an uncatchable OS kill/power loss.

Forward suppression and explicit forwarding use the documented
[OpenSSH forwarding options](https://man.openbsd.org/ssh_config#ClearAllForwardings)
and [multiplexed control requests](https://man.openbsd.org/ssh#O).


### Step 6: bootstrap API key validation

Enter the exact Organization ID and the complete temporary
`homestack-bootstrap` credential (`ID.secret`) prepared in Step 2. Validation
requires the current Steps 1–4 and the already connected, health-verified,
owned Step 5 SSH tunnel for the same server and API endpoint. A saved tunnel
history is insufficient. Step 6 never opens another connection or uses a public
API URL. A disconnected tunnel directs the user back to Step 5.

Tab/Shift+Tab cycles navigation, Organization ID, Temporary key, Verify and
Clear. Enter activates controls; **F6** verifies, **F5** clears the credential,
**F4** opens help and **F7** opens the sanitized report. **Esc** cancels a running
verification before leaving the wizard. Mouse fields and buttons use the same
checks. At 80x24, Tab enters the compact details page. The fixed mask reveals
neither token characters nor length and has no reveal option. Token editing is
append-only: Backspace removes the last character; Delete or Ctrl+U clears it.
Pastes trim surrounding whitespace, reject invalid characters and reject
oversized credentials rather than accepting a truncated token. Organization
IDs and key IDs accept only 1–128 ASCII letters, digits, underscores or hyphens;
secrets accept 1–512 of those characters, with exactly one separating dot.

All API operations are GET requests through the literal IPv4 loopback forward,
with the complete credential only in the Bearer authorization header:

- `/v1/org/{orgId}/api-keys?limit=100&offset=...` exhausts organization key
  pages. Success exercises authentication, the Root guard and `listApiKeys`.
- `/v1/org/{orgId}/api-key/{keyId}/actions?limit=100&offset=...` exhausts the
  bootstrap key's assigned actions and exercises `listApiKeyActions`. All four
  management actions must be present. `createApiKey` and `setApiKeyActions`
  are confirmed only as assigned metadata; no write permission is exercised.
- `/v1/org/{orgId}` confirms existence only on a matching successful response.
  Grant the recommended optional `getOrg` (Get organization) action to allow
  this confirmation. HTTP 403 is expected without it and does not fail validation.
  An empty key list alone never proves that an organization exists. HTTP 404 fails it.

An existing organization key named `homestack` is reported for deliberate
handling in Step 7; it is never duplicated, overwritten, regenerated or deleted.
The bootstrap credential is never revoked here. No Sites, Resources, Targets,
DNS records, policies or other Pangolin objects are changed.

Requests refuse redirects and ignore proxy configuration, validate response
schemas and pagination, limit decoded bodies to 1 MiB, and use a 10-second
request timeout and 60-second overall deadline. HTTP 401, HTTP 403 on required
reads, missing actions, invalid input, incompatible responses, cancellation,
timeout and tunnel loss have fixed sanitized diagnostics. Raw responses,
authorization headers and credentials are never displayed, logged or persisted.
Edits, cancellation and replaced tunnel sessions invalidate results; stale
worker completion cannot authorize later operations.

The credential and worker request buffers use zeroizing memory. Clear, wizard
exit, application exit, or invalidated server/integration metadata erase the
credential; cancellation also releases the worker's copy. Navigating to the
Step 7 placeholder keeps it in memory. Only the Organization ID and a sanitized
successful summary are saved atomically in the existing private
`integrations/pangolin.toml`; no separate credential store is created. Restart
restores historical information with an empty token and disconnected tunnel.
Progress reaches **6/8** only with a current successful validation and connected
tunnel. Phases 7 and 8 remain unavailable for execution.

Verification uses synthetic local API responses and disposable tunnel processes;
it does not establish compatibility with every Pangolin deployment or replace
validation against an explicitly authorized server. As with other process
memory, zeroization does not control operating-system swap or external memory
dumps.
