//! Turn the Python `config.toml` into `server.toml` and `client.toml`.

use hs_model::config::ConfigError;

use crate::legacy;

/// The two documents and what was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub server_toml: String,
    pub client_toml: String,
    /// Top-level tables and keys that went to neither file.
    pub dropped: Vec<String>,
}

/// Validate `text` like `config.load_config` and split it.
pub fn split(text: &str) -> Result<Split, ConfigError> {
    let read = legacy::read(text)?;
    let client_toml = read
        .client
        .to_toml()
        .map_err(|_| ConfigError::new("setup", "[setup] and [sounds] cannot be written back as TOML"))?;
    Ok(Split {
        server_toml: read.server.to_toml(),
        client_toml,
        dropped: read.dropped,
    })
}
