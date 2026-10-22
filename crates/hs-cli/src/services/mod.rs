//! Services an application repository declares in `.homestack/*.yaml`
//! (manifest schema 1): the parser, their discovery on GitHub, and their
//! containers in Proxmox, which alone record what HomeStack deployed.

pub mod deploy;
pub mod github;
pub mod instance;
pub mod lifecycle;
pub mod manifest;
pub mod migrate;
pub mod update;
