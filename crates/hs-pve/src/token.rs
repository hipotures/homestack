//! Proxmox API tokens: `USER@REALM!TOKENID=SECRET`.

use std::fmt;
use std::path::Path;
use zeroize::Zeroizing;

/// An API token. `Debug` never shows the secret.
#[derive(Clone)]
pub struct PveToken {
    id: String,
    secret: Zeroizing<String>,
}

/// Why a token was rejected. Messages never include the token text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the API token must look like USER@REALM!TOKENID=SECRET")]
    Format,
    #[error("could not read the API token file {path}: {reason}")]
    File { path: String, reason: String },
}

impl PveToken {
    /// Parse `USER@REALM!TOKENID=SECRET`, ignoring surrounding whitespace.
    pub fn parse(text: &str) -> Result<Self, TokenError> {
        let text = text.trim();
        if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(TokenError::Format);
        }
        let (id, secret) = text.split_once('=').ok_or(TokenError::Format)?;
        let (user, token_name) = id.split_once('!').ok_or(TokenError::Format)?;
        let (user_name, realm) = user.split_once('@').ok_or(TokenError::Format)?;
        if [user_name, realm, token_name, secret]
            .iter()
            .any(|part| part.is_empty())
        {
            return Err(TokenError::Format);
        }
        Ok(Self {
            id: id.to_owned(),
            secret: Zeroizing::new(secret.to_owned()),
        })
    }

    /// Read a token from a file holding exactly one token.
    pub fn from_file(path: &Path) -> Result<Self, TokenError> {
        let file_error = |reason: String| TokenError::File {
            path: path.display().to_string(),
            reason,
        };
        warn_if_readable_by_others(path);
        let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|error| file_error(error.kind().to_string()))?);
        Self::parse(&text)
    }

    /// `USER@REALM!TOKENID`, safe to log.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Value of the `Authorization` header.
    pub fn header_value(&self) -> Zeroizing<String> {
        // Best effort: HeaderValue/HTTP libraries own subsequent copies.
        Zeroizing::new(format!("PVEAPIToken={}={}", self.id, self.secret.as_str()))
    }
}

impl fmt::Debug for PveToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PveToken")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "secret file is accessible to other users; use mode 0600");
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}
