//! Types shared by the HomeStack server, its clients and the test tools.
//!
//! Everything here is plain data with `serde` and OpenAPI schemas, plus the
//! validation of `server.toml`. Nothing in this crate performs I/O.

pub mod capabilities;
pub mod config;
pub mod error;
pub mod health;
pub mod node;
pub mod pve;
pub mod server_config;

pub use error::{ErrorBody, ErrorCode, ErrorDetail, SCHEMA_VERSION};
pub use node::{NodeName, NodeNameError};
