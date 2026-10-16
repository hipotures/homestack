//! HomeStack configuration tools for the migration to the server.
//!
//! The Python CLI keeps one `config.toml` for everything. The server owns
//! part of it (`server.toml`, see `hs_model::config`) and the desktop the
//! rest (`client.toml`, see [`client`]). [`legacy`] reads the Python file
//! with the rules of `config.load_config`; [`split`] turns it into the two
//! new files. The `hs-config` command wraps both.

pub mod client;
pub mod legacy;
pub mod paths;
pub mod profiles;
pub mod split;
