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
appears under Remote Access as Planned and has no configuration or connection
operations yet.

The installer uses the DNS provider detected for the server domain and writes
the selected credential path and public DNS zone into `deploy.toml`. E2E uses
the Spaceship and TSIG files in the same directory by default; explicit
`--spaceship-file` and `--tsig-file` paths still select the requested files.
`homestack-deploy` uses the credential paths explicitly saved in its deployment
configuration. `tui.toml` contains interface preferences, not integration data.
