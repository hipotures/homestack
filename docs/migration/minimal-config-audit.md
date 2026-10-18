# Minimum installation input audit

The TUI installer now collects the configuration in its settings form and
does not require an input file. First installation asks for missing values;
reinstallation starts with the saved answers. It builds and validates native
server and client configuration without calling `hs_config::legacy::read`.
See [Client profiles](../client-profiles.md) for the current flow.

The following audit describes the legacy reader retained for Python parity.
Its smallest tested input is
[`config.minimal.toml`](../../config.minimal.toml). It has 21 leaf values;
removing any one of them makes that reader reject it.

| Section | Required values |
| --- | --- |
| Top level | `version` |
| `node` | `name` |
| `storage_layouts` | A nonempty storage list for the configured node's numeric suffix |
| `root` | `disk` |
| `home` | `disk`, `default_size` |
| `network` | `prefix`, `cidr`, `gateway`, nonempty `dns` |
| `cloud_init` | `snippet_storage`, `snippet_dir` |
| `user` | `name`, `uid`, `gid` |
| `workspace_ssh` | `user`, `identities_only`, `log_level` |
| `transport` | `type = "herdr"` |
| `transport.herdr` | Nonempty `workspace`, `tab` |

`identity_files`, `gold_vmid`, `control`, `repo`, `display`, `sounds` and `setup` can be
omitted. These requirements apply to the legacy reader only. The installer
derives node, Gold, storage layouts and snippet storage from its cluster
choices; workspace SSH user follows the configured guest user. The obsolete
transport fields are not needed or written by the installer.

The workspace identity filename is arbitrary. Parsing succeeds with
`/nonexistent/arbitrary-workspace-key`; it neither opens the key nor checks
its existence. By owner decision, `identity_files` is optional: omission or
an empty array means no workspace identities have been selected yet. No
example key path is inserted. Explicit entries still must be valid paths
without whitespace, duplicates or invalid types.
An actual workspace SSH connection still needs a usable key. The E2E
fixture uses `~/.ssh/e2e-workspace` because the controller generates that
key inside the disposable client's home. Neither input template contains
`identity_files`; the controller records its actual generated key when
it creates the lab's runtime configuration. That filename is not a parser
requirement and is not a production default.

The lab template additionally selects reference Gold 100; this makes it a
lab-specific fixture rather than the strict parser minimum. Its `@NODE@`
and `@LAYOUT@` placeholders are expanded before validation. It must not be
copied unchanged as an active TOML configuration.

## Verification

- `hs-config/tests/minimal.rs`: split the minimum into valid server and
  client documents; delete each of the 21 values in turn and check refusal;
  substitute an arbitrary absent key path; accept absent and empty identity
  lists; reject malformed explicit entries;
  accept the lab configuration without its reference Gold.
  Result: 4 passed.
- `hs-cli/tests/install_cli.rs`: server installation scenarios now use
  `config.minimal.toml` instead of Python's full example. These exercise
  the actual installer with fake Proxmox commands and deployment, plus an
  in-process HTTPS server on `pve-sim` for the final status.
  Result: 25 passed, 2 previously ignored slow tests not run. The HTTPS
  tests required permission to bind a local port outside the sandbox.
- No live Proxmox installation is claimed by this audit. At inspection,
  the registered E2E run was `failed`, its client configuration had been
  cleaned up, and there was no active paused lab to use.
