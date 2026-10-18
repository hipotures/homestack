# To do

- **Container replacement correctness.** Address the three confirmed
  findings in the [container replacement audit](container-replacement-audit.md):
  compare managed network properties semantically, verify container
  identity before destruction, and check configuration agreement after
  the entire selected batch. Temporary absence and manual rollback are
  accepted operating assumptions, not defects (owner, 2026-10-16).

- **Herdr tabs for the install: a setting for the mask.** The install's
  Herdr method offers only the tabs whose `WORKSPACE/TAB` name contains
  `pve` (any case), fixed in `crates/hs-cli/src/root_herdr.rs` (`TAB_MASK`).
  Make it a setting, e.g. in `client.toml`, with `pve` as the default
  (owner, 2026-10-15).
