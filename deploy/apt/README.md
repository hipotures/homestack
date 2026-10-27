# Signed HomeStack APT repository

The public archive is hosted by GitHub Pages:

```text
https://hipotures.github.io/homestack/apt/
```

It publishes `homestack-pve-agent` for amd64 PVE nodes, independently of Docker
images. Suite: `stable`. Component: `main`. The musl binary does not depend on
the node's glibc version. The package installs systemd integration and a dpkg
configuration file, enables boot startup, and restarts a configured service on
upgrades while respecting Debian service policy.

## Add the source once per node

Run as root on each node:

```sh
curl -fsSLo /tmp/homestack-apt-setup.sh https://hipotures.github.io/homestack/apt/setup.sh
bash /tmp/homestack-apt-setup.sh
apt-get install homestack-pve-agent
```

Prerequisites are `curl`, GnuPG and trusted CA certificates. The setup script
downloads the archive's public key over HTTPS, checks its pinned fingerprint,
and writes `/etc/apt/keyrings/homestack.gpg` plus
`/etc/apt/sources.list.d/homestack.sources`. It does not install the application
or change other repositories. The initial trust path is the project's HTTPS
Pages site and the fingerprint published in this repository.

Archive signing-key fingerprint:

```text
BDC607BB2F6BA396684B20A70A4E5AA38D54C9EE
```

For manual setup, download `homestack-archive-keyring.gpg` from the archive,
verify the fingerprint with `gpg --show-keys --with-fingerprint`, install it mode
0644 as `/etc/apt/keyrings/homestack.gpg`, then create:

```text
Types: deb
URIs: https://hipotures.github.io/homestack/apt
Suites: stable
Components: main
Architectures: amd64
Signed-By: /etc/apt/keyrings/homestack.gpg
```

Run `apt-get update` and `apt-get install homestack-pve-agent`. Select the local
Collector in `/etc/homestack/pve-agent.toml` before starting the service; new
package configuration intentionally has an empty endpoint. For pve1:

```toml
otlp_endpoint = "http://otel-1.homestack.app:4318/v1/metrics"
```

Use `otel-2` for pve2 and `otel-3` for pve3. Then run
`systemctl restart homestack-pve-agent`. See the
[agent documentation](../pve-agent/README.md) for configuration and removal of
an earlier manual installation before installing the package.

Subsequent updates use ordinary APT:

```sh
apt-get update
apt-get install --only-upgrade homestack-pve-agent
```

No SCP or repeated source/key configuration is needed. Updates are available
through APT; installing them is an explicit administrator action unless the
administrator configures an automatic update policy.

## Release and hosting

After compiling, reviewing, committing and pushing an agent/package change:

```sh
python3 scripts/tag-pve-agent.py
```

The helper fetches upstream tags and chooses the next patch in the independent
`pve-agent-X.Y.Z` line, starting at `0.1.0`. It requires a clean, pushed HEAD and
refuses a HEAD already released. Use `--dry-run` to preview. Never move tags or
overwrite package releases.

The existing CI workflow recognizes these tags and runs its release checks.
Only after successful checks does it build the static application, package it,
and publish `homestack-pve-agent_X.Y.Z_amd64.deb` in a GitHub Release. The APT
publication workflow downloads all non-draft, non-prerelease PVE agent packages,
checks available GitHub asset digests, generates `Packages` and `Packages.gz`,
and signs both `InRelease` and `Release.gpg`. Each version has a distinct pool
filename; previous versions remain downloadable. Metadata and pool checksums
are checked by APT using the scoped archive key.

GitHub Pages deploys the signed archive under `apt/`. Publication is serialized
and existing Pages content is owned by this workflow. GitHub Releases holds
the package files; Pages serves the APT index, keys and mirrored package pool.
An ordinary branch push runs checks only. To rebuild the archive from existing
releases without publishing a new package, run the separate **Publish signed
APT repository** workflow manually. A failed publication can also be retried
by rerunning the failed job, without replacing a package release.

The repository's Pages source must be GitHub Actions. The `github-pages`
environment allows `main` for manual publication and `pve-agent-*` tags for
releases. Its deployment job needs `pages: write` and `id-token: write`.

## Signing key maintenance

`deploy/apt/homestack-archive-keyring.gpg` and `signing-key.fingerprint` are
public. The armored private signing key is stored in the repository's Actions
secret `APT_SIGNING_KEY`. It is imported into a temporary mode-0700 GnuPG home
only during publication; it is never included in Pages artifacts or repository
content. The publisher refuses a secret whose fingerprint does not match the
committed key. Keep an offline backup of the private key and monitor its expiry.
The initial maintainer copy was generated under
`~/.local/share/homestack/apt-signing/` on the release workstation.

Key changes must update the committed public key, fingerprint, bootstrap pin,
documentation and Actions secret together. Already configured clients require
an explicitly planned key update before a different signing key is used.

Local packaging and archive checks can use disposable output directories:

```sh
deploy/pve-agent/build-static.sh
python3 deploy/pve-agent/build-deb.py --version 0.0.0 \
  --binary target/x86_64-unknown-linux-musl/release/homestack-pve-agent \
  --output target/debian-local
python3 deploy/apt/build-repository.py --packages target/debian-local \
  --output target/apt-local/apt --gnupg-home PATH_TO_SIGNING_KEY_HOME
gpgv --keyring "$PWD/deploy/apt/homestack-archive-keyring.gpg" \
  target/apt-local/apt/dists/stable/InRelease
```

These scripts refuse to overwrite existing packages or a nonempty archive.
Use a fresh output directory for each packaging experiment. Do not publish the
local `0.0.0` diagnostic package. A local APT download/signature check does not
prove installation or reboot behavior on PVE.
