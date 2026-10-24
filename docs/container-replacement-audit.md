# Container replacement audit

**Date:** 2026-10-16
**Reviewed revision:** `fc1e0395174b`
**Scope:** replacement of the HomeStack server LXC containers through
`homestack-deploy apply` and the TUI reinstall action, not workspace VM
refresh.

The basic replacement path succeeded against local API stand-ins. Three
correctness issues remain open. Production was inspected read-only; no
production container was stopped, destroyed or recreated during this audit.

## Accepted operating model

The owner confirmed these assumptions after reviewing the initial findings:

- Replacement is sequential and destructive: stop and remove the old
  container, then create its replacement with the same CTID and IP address.
- A temporarily missing container, including after a failed replacement,
  is expected. The remaining instances on other PVE nodes serve clients;
  failover can increase response latency.
- Rollback is manual. Automatic rollback and keeping two copies of an
  instance running during replacement are not requirements.
- Reusing the CTID and IP explains why the current implementation cannot
  simply start a second copy alongside the first. This is an implementation
  constraint, not a verified historical reason for the design decision.
- Destruction removes the container's volumes. The replacement receives
  its certificate, credentials and configuration again; preserving
  arbitrary container-local data is not part of this path.

Client failover applies to reads and choosing an instance for a new plan.
A plan, its apply and its job stay on the instance that accepted the plan.
An unavailable instance is reported rather than automatically replaying a
possibly destructive operation elsewhere. Replacement is therefore not
transparent to in-flight operations. See
[`hs-client`](../crates/hs-client/src/lib.rs), particularly `Client::first`
and `Session`.

The initial concern about a missing container after an image-pull failure
is withdrawn as a defect under this operating model. Downloading the image
before removal could reduce the outage window, but is not a requirement
for automatic recovery or overlapping containers.

## Current replacement flow

In the TUI, Space marks server containers. Enter opens image selection and
then a confirmation plan. The action processes selected instances one at
a time through `hs_deploy::deploy::apply` and records the selected image
in `deploy.toml` only after the entire action succeeds.

For each selected instance, `apply` currently:

1. Loads the certificate, token files and local `server.toml`.
2. Finds the existing LXC by CTID. A running container whose description
   and `net0` match is only configured again.
3. Otherwise stops and destroys the existing container and its volumes.
4. Ensures the image exists in the node's template storage, pulling it
   when necessary, and creates the replacement LXC.
5. Sets its environment, starts it and waits for an HTTPS health response.
6. Removes certificate bootstrap variables from the Proxmox configuration,
   restarts when the environment change is pending, and checks removal.
7. Sends the Proxmox token and `server.toml` through the instance API.
8. Checks configuration versions across every configured instance.

Sources: [`deploy.rs`](../crates/hs-deploy/src/deploy.rs),
[`node.rs`](../crates/hs-deploy/src/node.rs),
[`reinstall.rs`](../crates/hs-cli/src/tui/reinstall.rs).

## Resolution of findings (2026-10-19)

### 1. Network comparison causes unnecessary destruction — Resolved

`node::unchanged` previously compared `net0` as an exact string. Proxmox
reorders network properties and adds generated fields such as `hwaddr`,
`type` and `host-managed`. Equivalent network settings consequently
compared unequal, causing instances that already had the target image
(for example `api1` in an `X, Y, Y` cluster) to be destroyed and
recreated unnecessarily.

**Fix:** [`node::same_net`](../crates/hs-deploy/src/node.rs) compares
managed `net0` properties (`name`, `bridge`, `ip`, `gw`, etc.) by key
and value, ignoring key ordering and Proxmox-generated properties
(`hwaddr`, `type`, `host-managed`). When the target image changes,
`description` differs and the old container is still stopped and
destroyed before creating the replacement so CTID and IP are released
cleanly.

### 2. Destruction does not verify the instance's identity — Rejected (not applicable)

**Why rejected:** `homestack-deploy` authenticates exclusively with the
`!deploy` token (`HomeStackDeploy`), whose VM authority is scoped to
`/pool/homestack-service` and `/pool/homestack-test`. The service pool contains
API servers and deployed services; production workspace VMs live in
`/pool/homestack` and Gold in `/pool/homestack-gold`. Proxmox limits container
visibility and stop/delete authority to members of the two authorized pools,
so this boundary does not isolate individual services or guests inside the
test pool. See [Proxmox permissions](proxmox-permissions.md) for the separate
storage authority and privilege-separated user/token intersection.

### 3. TUI batch validation runs before the batch is complete — Resolved

`reinstall::work` calls `deploy::apply` sequentially for each selected
instance so that a failure on `api1` halts immediately while `api2` and
`api3` remain running on their previous containers. Previously, each
single-instance `deploy::apply` ended by running `check_versions` across
*all* instances in `deploy.toml`, including instances not yet updated in
the current batch.

**Fix:**
- [`deploy::configure`](../crates/hs-deploy/src/deploy.rs) immediately
  verifies `GET /v1/config/version` on the instance just updated and
  aborts on mismatch before moving to the next node.
- `deploy::apply` checks version agreement across the `selected`
  instances of that `apply` invocation.
- [`reinstall::work`](../crates/hs-cli/src/tui/reinstall.rs) runs
  [`deploy::check_selected_versions`](../crates/hs-deploy/src/deploy.rs)
  across the entire selected batch after all selected instances have been
  processed.

## Verification evidence

At inspection time, `api1`, `api2` and `api3` were running on `pve1`,
`pve2` and `pve3`, respectively. All reported configuration version
`ee82a4dfb6bc` and image `0.1.0-a1945f2ece03`.

The following checks passed: 26 local tests and one read-only live test.

```sh
cargo test --locked --offline -p hs-deploy
cargo test --locked --offline -p hs-cli --test tui_reinstall
cargo test --locked --offline -p hs-cli --lib tui::reinstall
cargo test --locked --offline -p hs-cli --test live_reinstall -- --ignored --nocapture
```

The destructive live deployment gate was not run. The live test above
only reads the cluster and published registry images. Additional
read-only API requests checked the installed containers' descriptions,
network settings and mount points.

Five isolated scenarios used local HTTPS API stand-ins, dummy tokens
and temporary configuration files:

| Scenario | Observed result | Assessment |
|---|---|---|
| Basic replacement | Created, started, configured; certificate bootstrap variables removed | Passed in simulation |
| Repeat with identical inputs and normalized `net0` | Container destroyed and recreated | Finding 1 |
| Image pull fails after removal | Original container absent, apply fails | Accepted; manual recovery |
| Unrelated container occupies the configured CTID | Unrelated container destroyed | Finding 2 |
| First of two instances receives a new configuration | Global mismatch aborts the single-instance apply | Finding 3 |

The temporary stand-ins were not added to the repository. These scenarios
are evidence from the audit, not existing permanent regression coverage.
No implementation changes or fixes are included in this document.
