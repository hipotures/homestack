//! `homestack-deploy`: the server instances on the Proxmox nodes, their
//! certificate and their configuration, through the Proxmox API and the
//! server's own API only (`docs/migration/phase-11.md`).

pub mod acme;
pub mod config;
pub mod deploy;
pub mod dns;
pub mod node;
pub mod ovh;
pub mod registry;
pub mod spaceship;
