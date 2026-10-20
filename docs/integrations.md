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

## Pangolin setup: server, prerequisites, discovery and Integration API

Open **Settings > Integrations > Remote Access > Pangolin** to enter the setup
wizard. All eight stages are shown: Pangolin server, Pangolin prerequisites,
Pangolin discovery, Integration API, API tunnel, Bootstrap API key, HomeStack API
key, and Integration verification. The first four stages are implemented;
stages 5–8 remain locked and cannot execute operations.

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
navigation shows all eight steps. No later stage is executable.

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
**Step 5 remains locked:** no tunnel is opened and no API key is requested.

The unauthenticated health contract is checked against the
[Pangolin v1.24.0 Integration API router](https://github.com/fosrl/pangolin/blob/1.24.0/server/routers/integration.ts).
