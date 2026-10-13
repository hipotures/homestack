//! Turn the Python `config.toml` into `server.toml` and `client.toml`.

use hs_model::config::ConfigError;

use crate::client::{DEFAULT_TOKEN_FILE, Server};
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
    split_with_server(text, &[])
}

/// As [`split`], with `[server] urls` in `client.toml` when `urls` is not
/// empty (the Python file has no such section). The result is checked like
/// a written `client.toml`, so a URL that is not `https://` is refused.
pub fn split_with_server(text: &str, urls: &[String]) -> Result<Split, ConfigError> {
    let mut read = legacy::read(text)?;
    if !urls.is_empty() {
        read.client.server = Some(Server {
            urls: urls.to_vec(),
            token_file: DEFAULT_TOKEN_FILE.to_owned(),
            ca_file: None,
        });
    }
    let client_toml = read
        .client
        .to_toml()
        .map_err(|_| ConfigError::new("setup", "[setup] and [sounds] cannot be written back as TOML"))?;
    if !urls.is_empty() {
        crate::client::ClientConfig::parse(&client_toml)?;
    }
    Ok(Split {
        server_toml: read.server.to_toml(),
        client_toml,
        dropped: read.dropped,
    })
}
