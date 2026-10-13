//! Bodies of the `/v1/config` routes.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Body of `GET /v1/config/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigVersion {
    /// SHA-256 of the stored `server.toml`, lowercase hex; `null` before the
    /// first upload.
    pub config_version: Option<String>,
}

/// Body of a successful `PUT /v1/config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigUpdated {
    /// SHA-256 of the stored text, also sent as the `ETag`.
    pub config_version: String,
    /// `false` when the text was already stored; nothing was written then.
    pub changed: bool,
}

/// Body of `PUT /v1/secrets/tls`: the certificate Caddy serves, PEM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsCertificate {
    /// The certificate chain, leaf first.
    pub cert: String,
    /// Its private key.
    #[schema(value_type = String)]
    pub key: crate::jobs::Secret,
}
