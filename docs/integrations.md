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

## Pangolin setup: SSH server and manual prerequisites

Open **Settings > Integrations > Remote Access > Pangolin** to enter the setup
wizard. All eight stages are shown: Pangolin server, Pangolin prerequisites,
Pangolin discovery, Integration API, API tunnel, Bootstrap API key, HomeStack API
key, and Integration verification. The first two stages are implemented;
stages 3–8 remain locked and cannot execute operations.

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

These phases do not inspect Pangolin, Docker or administrative permissions,
configure an API or tunnel, bootstrap credentials, or create remote objects.
It changes no VPS configuration. A successful SSH test does not establish that
Pangolin is installed or that any later integration stage works.

### Manual prerequisites (Step 2)

After Step 1 verifies and saves the current server, select **Pangolin
prerequisites**. Expand **Pangolin installation** and **Bootstrap API key** with
Enter or Right; Left collapses a header. Tab/Shift+Tab cycles navigation, the two
headers and the single confirmation checkbox. Space or Enter toggles the checkbox;
clicking headers and the checkbox performs the same actions. F4 opens scrollable
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

The checkbox confirms both instruction groups. **Awaiting confirmation** becomes
**Confirmed by user**, and progress changes from **1/8** to **2/8**, only after an
atomic private write to the existing `integrations/pangolin.toml`. Unchecking
persists the incomplete state. Confirmation is associated with the exact verified
server target; editing the target invalidates it, and saving a changed target
removes the old confirmation. Unrelated settings are preserved. Write failures
leave confirmation unchanged and display an error; Space retries. Confirmation
restores on restart, but never claims technical verification of Pangolin or its
API.

**Pangolin discovery** (Step 3) remains unimplemented. Its future read-only SSH
inventory will identify installation/running state, version, actual installation
method, container/service identities, Compose/service and application configuration
paths, listening ports and mappings, and Integration API port and enabled state.
It will report ambiguous or unsupported layouts without modifying them. It will
not assume Docker Compose, fixed names or paths. Any subsequent configuration
change belongs to Step 4, with explicit approval and backup.
