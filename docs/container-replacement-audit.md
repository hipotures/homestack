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

## Open findings

### 1. Network comparison causes unnecessary destruction

`node::unchanged` compares `net0` as an exact string. Proxmox reorders
network properties and adds generated fields such as `hwaddr`, `type`
and `host-managed`. Equivalent network settings consequently compare
unequal.

Read-only inspection confirmed that all three installed instances had
the expected description and image, but different raw `net0` strings.
A repeated local apply with the same image, certificate and managed
network settings stopped, destroyed and recreated the container again.

Compare the managed network properties by key and value, independently
of ordering and generated properties. Regression coverage should prove
that equivalent Proxmox-normalized settings do not trigger destruction,
while an actual managed network change is still detected.

### 2. Destruction does not verify the instance's identity

`node::container` selects an LXC by CTID. The destructive apply path does
not verify its expected hostname or managed-server identity before
removal. A local scenario with an unrelated hostname and tag at the
configured CTID still stopped and deleted that container.

The deploy token's pool permissions limit visibility, but do not protect
another service inside that accessible pool. The install planner has
collision checks in [`servers.rs`](../crates/hs-cli/src/servers.rs);
direct deployment and TUI reinstall do not repeat those checks.

Verify the expected instance identity before any destructive mutation.
A collision regression test should assert that no stop or delete request
is issued for an unrelated container at the configured CTID.

### 3. TUI batch validation runs before the batch is complete

`reinstall::work` calls `deploy::apply` separately for each selected
instance. Each call ends with `check_versions`, which checks every
configured instance, not just the instance being processed.

When local `server.toml` differs from the configuration on the remaining
instances, the first successful replacement uploads the new configuration
and then fails the global consistency check. The TUI exits before
updating the remaining selected instances. An unavailable later instance
can similarly prevent the batch from reaching it.

A local two-instance scenario reproduced the error
`the instances hold 2 configuration versions` after successfully updating
only the first instance.

Keep per-instance readiness checks, but perform the batch-wide consistency
check after all selected instances have been processed. Add a regression
test for a multi-instance reinstall that changes the configuration.

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
