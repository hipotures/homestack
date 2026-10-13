//! Proxmox VE API client.
//!
//! Talks to `https://<node>:8006/api2/json` with an API token, verifies TLS
//! against the system roots plus an optional extra CA, and fails over between
//! the configured endpoints. Any node with quorum serves the whole cluster API
//! (it proxies `/nodes/{node}/...` itself), so endpoints are interchangeable.

mod client;
mod error;
mod token;

pub use client::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, NodeEntry, NodeState, PveClient, PveClientConfig};
pub use error::{EndpointFailure, PveError};
pub use token::{PveToken, TokenError};
