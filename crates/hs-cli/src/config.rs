//! The client's configuration: `client.toml` (`hs-config`), the client
//! token and how to reach the server instances.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use hs_config::client::{ClientConfig, Server};

use crate::{AppError, expand_home};

/// `client.toml` read and checked, with where it is.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub client: ClientConfig,
}

/// `$XDG_CONFIG_HOME/homestack/client.toml`, else
/// `~/.config/homestack/client.toml`.
#[must_use]
pub fn default_path(home: &Path, config_home: Option<&Path>) -> PathBuf {
    let base = config_home.map_or_else(|| home.join(".config"), Path::to_owned);
    base.join("homestack").join("client.toml")
}

/// Read and check `path`.
pub fn load(path: &Path) -> Result<Loaded, AppError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::new(format!(
                "Configuration file {} was not found. Create it with `hs-config split --server-url https://api.example.net` \
or write it by hand (docs/migration/phase-13.md).",
                path.display()
            ))
        } else {
            AppError::new(format!("Could not read {}: {error}", path.display()))
        }
    })?;
    let client = ClientConfig::parse(&text)
        .map_err(|error| AppError::new(format!("Invalid configuration {}: {error}", path.display())))?;
    Ok(Loaded {
        path: path.to_owned(),
        client,
    })
}

impl Loaded {
    /// `[server]`, required by every command that talks to the server.
    pub fn server(&self) -> Result<&Server, AppError> {
        self.client.server.as_ref().ok_or_else(|| {
            AppError::new(format!(
                "Missing [server] in {}: add urls = [\"https://api.example.net\"] (the HomeStack server instances).",
                self.path.display()
            ))
        })
    }

    /// The `hs-client` configuration: HTTPS URLs only (decision 8), the
    /// token from a file only its owner can read, the extra CA if set.
    pub fn client_config(&self, home: &Path) -> Result<hs_client::ClientConfig, AppError> {
        let server = self.server()?;
        let mut endpoints = Vec::with_capacity(server.urls.len());
        for url in &server.urls {
            let parsed: url::Url = url
                .parse()
                .map_err(|error| AppError::new(format!("Invalid server URL {url}: {error}")))?;
            if parsed.scheme() != "https" {
                return Err(AppError::new(format!(
                    "Server URL {url} must start with https://; the client token is sent only over TLS"
                )));
            }
            endpoints.push(parsed.into());
        }
        let token_path = expand_home(&server.token_file, home);
        let token = read_private(&token_path, "client token")?;
        let token = token.trim().to_owned();
        if token.is_empty() {
            return Err(AppError::new(format!(
                "The client token file {} is empty",
                token_path.display()
            )));
        }
        let mut config = hs_client::ClientConfig::new(endpoints, token);
        if let Some(ca_file) = &server.ca_file {
            let path = expand_home(ca_file, home);
            config.extra_ca_pem =
                Some(std::fs::read_to_string(&path).map_err(|error| {
                    AppError::new(format!("Could not read the CA file {}: {error}", path.display()))
                })?);
        }
        Ok(config)
    }
}

/// A secret file that only its owner may read or write.
fn read_private(path: &Path, what: &str) -> Result<String, AppError> {
    let meta = std::fs::metadata(path)
        .map_err(|error| AppError::new(format!("Could not read the {what} file {}: {error}", path.display())))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(AppError::new(format!(
            "The {what} file {} is readable by others; chmod 600 it",
            path.display()
        )));
    }
    std::fs::read_to_string(path)
        .map_err(|error| AppError::new(format!("Could not read the {what} file {}: {error}", path.display())))
}

/// Save the SSH sound switch while preserving the rest of client.toml.
pub fn save_ssh_sounds(path: &Path, enabled: bool) -> Result<(), AppError> {
    use std::io::Write;

    let save = || -> Result<(), Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)?;
        let mut document: toml_edit::DocumentMut = text.parse()?;
        document["sounds"]["enabled"] = toml_edit::value(enabled);
        ClientConfig::parse(&document.to_string())?;
        let parent = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(document.to_string().as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    };
    save().map_err(|_| AppError::new(format!("Could not save SSH sounds in {}", path.display())))
}
