# HS-08 obsolete-code and documentation review

**Date:** 2026-10-24

**Baseline:** `c1a540b` on `main`, matching `origin/main` when reviewed.

**Scope:** source/documentation inspection and local client verification only.

## Removed helper

`crates/hs-cli/src/services/lifecycle-root.pl` was an unreferenced source file
introduced in `a2ca4f5`. Before removal, tracked-file searches found no reference
to its path, basename or helper-only action names outside the helper itself.
It was mode 100644, had no executable wrapper or documented operator command,
and was not a registered Cargo target or embedded Rust resource.

The review covered workspace/crate manifests, `hs-cli/build.rs`, all tracked
`include_str!`/`include_bytes!` resources, CLI/TUI dispatch, installer and
standalone deploy entry points, shell scripts, container build files and CI
packaging. Container builds copy the source tree to compile Rust; they do not
install or execute this helper. Supported images install compiled binaries and
explicit runtime service files. Workspace packages are not published to Cargo.
No supported packaging or operator invocation path to this helper was found.

The active paths remain `services/lifecycle.rs` for PVE API maintenance,
`services/update.rs` and `hs_deploy::recreate` for replacement, and
`hs_deploy::deploy` for standalone/installer API deployment. The Perl helper
was not used to replace any of them.

## Documentation findings

- `services/deploy.rs` allocates `mp0` only for manifest persistence; the current
  API manifest is stateless and the Pangolin manifest has a persistent home.
- `hs_deploy::recreate` moves the original home through CT 999999 and back before
  starting the replacement. Common updates retain settings through the holder
  even without a home. Update metadata retains operation and power intent.
- `services/lifecycle.rs` explicitly removes root and home after typed-name
  confirmation. There is no separate retain-home service uninstall command.
- Common service identity is in PVE descriptions; `homestack-server` is the
  current container tag, distinct from manifest ID `homestack-api`.
- Setup and repository commands dispatch to current Rust client modules while
  retaining their embedded Python guest programs. User help now describes those
  supported commands, and configuration errors point to existing documentation.
- The missing migration-page link in E2E documentation now points to the current
  workspace lifecycle reference; recorded E2E evidence is preserved.

The former persistent-home TODO checklist was replaced with implemented status,
remaining live-verification work and current limitations. Active internal-service
instructions distinguish current stateless APIs from earlier home-backed API
instances. The dated replacement audit and managed-API live report are retained
as historical evidence, without rewriting their findings to describe today's
implementation. Current manifest examples already express both home modes and
need no deployment changes.

## Retained components and local archival candidate

`hs-config`, standalone `homestack-deploy`, embedded guest/Pangolin Python,
`pve-sim`, Pangolin Step 8 state/integration, bootstrap dependencies and the E2E
infrastructure remain. Their reachability or deliberate verification role
prevents treating them as dead code merely because of historical names.

The local `~/.config/homestack/services.toml` exists but is not tracked. No
tracked source references `services.toml`; current discovery reads repository
`.homestack/*.yaml` manifests and inventories deployments through PVE metadata.
Ownership and external consumers of that local file are not established. It is
an archival candidate for a separate operator decision, not a cleanup target.
Its contents were not needed for this review and the file was left untouched.

No live infrastructure, templates, volumes, credentials or runtime configuration
were changed. No API or Pangolin image release is needed: the changes affect
documentation, client messages and an unused client source file only. The root
Containerfile changes are comments only.

## Local verification

The required `python3 scripts/cargo.py build --locked -p hs-cli --bin homestack`
completed successfully and retained `target/debug/homestack`. Rust formatting,
diff whitespace and local Markdown link targets were checked. Temporary
subprocess checks exercised the compiled help and missing-configuration error
using a nonexistent configuration path in a temporary directory. No permanent
tests or live-infrastructure checks were added or run.
