# Local Pangolin experiment

For an already provisioned experiment, use the following commands manually from the repository root, outside the agent session. They reuse the recorded Site, Resource, Target and CNAME. They do not provision, discover domains/backends, access OVH, bootstrap credentials or modify the VPS.

Start the desktop connection in the background using **Pangolin CLI**:

```bash
python3 tools/pangolin-test/harness.py start
```

After `start` returns, independently verify current Site and Target health:

```bash
python3 tools/pangolin-test/harness.py status
```

Stop only this experiment's recorded desktop process:

```bash
python3 tools/pangolin-test/harness.py stop
```

`start` verifies existing object identities, starts `pangolin up site --config-file ...` in a separate session and waits for current Site/Target health. It leaves the process running after the harness exits, including when health times out. Run `status` or `stop` to recover. Repeated start reuses the same owned PID. A Site already Online without a matching local ownership record blocks startup rather than risking a duplicate.

`status` reads live Site Online, Target `hcHealth`, Resource/Target enabled state and exact Target membership. It checks twice, two seconds apart. `healthy_after_start_return: true` requires both observations to be healthy, the owned CLI process to exist, and the original start command to have exited. Historical uptime and saved health strings are not used. Public certificate verification and unauthenticated HTTP behavior are reported separately under `public_tls` and `public_authentication_required`. A healthy connector is not proof of an authenticated public HTTP round trip.

The public URL comes from saved experiment state and appears in the report. Open it in a browser and sign in through Pangolin SSO while the background CLI is running. The expected health response is `{"status":"ok"}`. The organization Integration API key cannot authenticate public Resources. No session cookies or browser credentials are requested or read by the harness.

`stop` needs no Integration API token or network connection. It verifies UID, boot ID, PID start time, process session, executable and exact arguments, then uses a Linux pidfd to signal only that process. It never uses `killall`, `pkill` or broad process-group termination. Stopping twice is harmless. It retains credentials for a later start. An inconsistent ownership record or held lock blocks adoption/start/stop; preserve state for diagnosis rather than deleting lock files around a running process.

The CLI executable is the pinned official artifact, verified by SHA-256. No standalone `newt` executable is downloaded or launched. Pangolin 1.24 still names the Site API type `newt` and its credential field `newtId`; these wire fields remain required for the existing Site. The supported CLI embeds the Site connector ([official CLI source](https://github.com/fosrl/cli/blob/0.18.1/cmd/up/site/site.go)).

Credentials and control state stay under `~/.local/state/homestack/pangolin-test`, outside Git, with directory mode `0700` and file mode `0600`. The detached CLI inherits a lifetime lock to prevent duplicate startup. It has no terminal, inherited token/proxy/debug environment, or persistent stdout/stderr logs. Its regular-file write limit is zero, preventing its embedded updater from replacing the verified executable; configuration is written securely before launch. Socket/device writes remain available. This test mode disables optional client, SSH and metrics features. It does not install a service or start automatically after reboot. Filesystem permissions do not isolate secrets from other processes under the same OS account.

Environment metadata is read from the existing private `~/.config/homestack/pangolin-test.toml`. This fictional example shows its required fields:

```toml
api_url = "http://127.0.0.1:18080/v1"
organization = "example-org"
server_url = "https://dashboard.example.invalid"
backend_instance = "backend"
```

The existing organization token stays in `~/.config/homestack/secrets/pangolin.json` as raw `ID.secret`, mode `0600`. Real values must stay outside Git. `--settings-file`, `--credential-file` and `--state-dir` override local file paths.

`run` remains the explicitly approved provisioning experiment for a new state directory. It performs read-only discovery and displays a complete plan before Site and Resource approvals. It uses the local HomeStack `deploy.toml` and `client.toml` metadata and the OVH file referenced by `acme.ovh_file`, or the standard `integrations/ovh-dns` beside `deploy.toml`. It creates only its recorded CNAME/Site/protected Resource/exact health Target, then performs checks with a bounded foreground CLI. It does not leave that foreground CLI running; use `start` for persistent operation. Neither `run` nor `cleanup` can proceed while the detached connector is running.

To remove recorded test objects, stop the connector first, then explicitly approve cleanup:

```bash
python3 tools/pangolin-test/harness.py cleanup
```

Cleanup validates ownership/dependencies before deleting the exact recorded Target, Resource, Site and CNAME. A changed/unrecorded DNS record is never adopted or deleted; no DNS zone is deleted. The separate, manually approved VPS Caddy addition is outside harness cleanup and must be removed separately by its exact markers. Do not restore a whole old Caddy backup over subsequent unrelated edits.

Exit codes: `start` returns 0 when a connector is running and initial Site/Target health was observed; persistence is still pending an independent `status`. `status` returns 0 only for `healthy_after_start_return: true`, otherwise 2. Public TLS and authenticated public health remain separately reported. Failures return 1; `stop` and cleanup return 0 on completion/no-op. The old provisioning `run` returns 2 when authenticated public health remains unresolved.

Compatibility: Linux with `/proc` and pidfd support, Python 3.11+, Pangolin 1.24.0 and official CLI 0.18.1 on amd64/arm64. Offline verification uses fake local API servers, a tiny fake ELF CLI and synthetic credentials only. See [VERIFICATION.md](VERIFICATION.md).
