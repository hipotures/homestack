//! Client errors.

/// A Proxmox API call failed.
///
/// Messages come from Proxmox or from the TLS/HTTP stack; they never contain
/// the API token.
#[derive(Debug, thiserror::Error)]
pub enum PveError {
    /// The client configuration is unusable.
    #[error("invalid Proxmox client configuration: {0}")]
    Config(String),
    /// No endpoint could be connected to, so the request was never sent.
    #[error("no Proxmox endpoint is reachable")]
    Unreachable(Vec<EndpointFailure>),
    /// Proxmox rejected the token (HTTP 401).
    #[error("Proxmox rejected the API token")]
    Unauthorized,
    /// The token lacks a privilege (HTTP 403).
    #[error("permission denied: {message}")]
    Forbidden { message: String },
    /// Proxmox could not proxy the request to the target node (HTTP 595).
    #[error("node unreachable: {message}")]
    NodeUnreachable { message: String },
    /// Any other HTTP error from Proxmox.
    #[error("Proxmox error {status}: {message}")]
    Api { status: u16, message: String },
    /// The request was sent but the response was lost or unusable.
    #[error("bad Proxmox response: {0}")]
    Response(String),
}

/// Why one endpoint could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointFailure {
    /// Endpoint origin, e.g. `https://pve1.example:8006`.
    pub endpoint: String,
    /// Connection or TLS error chain.
    pub detail: String,
}
