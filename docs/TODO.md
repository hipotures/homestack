# To do

- **Herdr tabs for the install: a setting for the mask.** The install's
  Herdr method offers only the tabs whose `WORKSPACE/TAB` name contains
  `pve` (any case), fixed in `crates/hs-cli/src/root_herdr.rs` (`TAB_MASK`).
  Make it a setting, e.g. in `client.toml`, with `pve` as the default
  (owner, 2026-10-15).

- **Deploying a new server image without editing deploy.toml.**
  `homestack-deploy apply` takes the image from `deploy.toml`, so a new
  image means editing its `image` line by hand. Give `apply` an
  `--image TAG` option, or let it take the image of the current commit
  (`0.1.0-<sha12>`, as CI tags it), and write the new tag back to
  `deploy.toml`.
