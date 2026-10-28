//! Secrets sent through the API (mapping 8.1, `phase-11.md`): write-only
//! routes that keep them in the instance's state directory, mode `0600`.
//!
//! - `secrets/pve-token`: the Proxmox token, used at once and at every
//!   start (unless `HOMESTACK_PVE_TOKEN_FILE` names one).
//! - `tls/cert.pem`, `tls/key.pem`: the certificate Caddy serves. The
//!   environment's certificate is stored at the first start only; a new one
//!   comes through `PUT /v1/secrets/tls`, and Caddy is told to load it.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use hs_model::ErrorCode;
use hs_model::jobs::Secret;
use hs_model::server_config::TlsCertificate;
use hs_pve::PveToken;

use crate::AppState;
use crate::error::ApiError;

/// Largest secret request accepted.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// A certificate chain and its key, PEM.
#[derive(Clone)]
pub struct TlsPair {
    pub cert: String,
    pub key: Secret,
}

impl std::fmt::Debug for TlsPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsPair(<redacted>)")
    }
}

impl TlsPair {
    /// Both PEM blocks, as far as the server can tell; Caddy checks the rest.
    pub fn checked(cert: String, key: Secret) -> Result<Self, &'static str> {
        if !(cert.contains("-----BEGIN CERTIFICATE-----") && cert.contains("-----END CERTIFICATE-----")) {
            return Err("The certificate must be a PEM certificate chain.");
        }
        let key_text = key.expose();
        if !(key_text.contains("-----BEGIN ") && key_text.contains("PRIVATE KEY-----")) {
            return Err("The key must be a PEM private key.");
        }
        Ok(Self { cert, key })
    }
}

fn tls_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("tls")
}

fn secrets_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("secrets")
}

/// Serializes the secret updates of the API: each one reads, writes and
/// swaps in its state alone.
static SECRET_WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The new content of a secret file, written and synced next to it under a
/// unique name; removed when dropped unless [`Staged::commit`] renamed it.
struct Staged {
    temporary: PathBuf,
    path: PathBuf,
}

impl Staged {
    /// Write `bytes` for `path`, mode `0600`, in a `0700` directory; the
    /// file at `path` is not touched.
    fn new(path: &Path, bytes: &[u8]) -> std::io::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| std::io::Error::other("no parent directory"))?;
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let name = path
            .file_name()
            .ok_or_else(|| std::io::Error::other("no file name"))?
            .to_string_lossy();
        let staged = Self {
            temporary: dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4())),
            path: path.to_owned(),
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged.temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(staged)
    }

    /// Replace the file at `path` with the staged one in one rename. The
    /// rename is the change: a failed sync of the directory afterwards is
    /// only logged, so an error always means the old file is still there.
    fn commit(&self) -> std::io::Result<()> {
        std::fs::rename(&self.temporary, &self.path)?;
        if let Some(dir) = self.path.parent()
            && let Err(error) = std::fs::File::open(dir).and_then(|dir| dir.sync_all())
        {
            tracing::warn!(%error, "could not sync the secrets directory");
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}

/// Write `bytes` to `path` atomically, mode `0600`, in a `0700` directory.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    Staged::new(path, bytes)?.commit()
}

/// How to put one file of the TLS pair back as it was: its old content
/// staged, or no file.
enum Restore {
    Staged(Staged),
    Absent(PathBuf),
}

impl Restore {
    fn new(path: &Path) -> std::io::Result<Self> {
        Ok(match read_optional(path)? {
            Some(text) => Self::Staged(Staged::new(path, text.as_bytes())?),
            None => Self::Absent(path.to_owned()),
        })
    }

    fn apply(&self) -> std::io::Result<()> {
        match self {
            Self::Staged(staged) => staged.commit(),
            Self::Absent(path) => match std::fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            },
        }
    }
}

/// Put the previous key and certificate back after a failed replacement.
fn restore_pair(restore: &[Restore]) {
    for file in restore {
        if let Err(error) = file.apply() {
            tracing::error!(%error, "could not put the previous certificate back");
        }
    }
}

fn read_optional(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Store the environment's certificate unless one is stored already (a
/// renewed one must not be replaced by the older one at a restart).
pub fn store_bootstrap_tls(state_dir: &Path, pair: Option<&TlsPair>) -> Result<(), String> {
    let Some(pair) = pair else {
        return Ok(());
    };
    let dir = tls_dir(state_dir);
    if dir.join("cert.pem").exists() {
        tracing::info!("a certificate is stored; the one in the environment is not used");
        return Ok(());
    }
    write_private(&dir.join("key.pem"), pair.key.expose().as_bytes())
        .and_then(|()| write_private(&dir.join("cert.pem"), pair.cert.as_bytes()))
        .map_err(|error| format!("could not store the certificate in {}: {}", dir.display(), error.kind()))?;
    tracing::info!("certificate of the environment stored");
    Ok(())
}

/// The Proxmox token sent earlier, if any.
pub fn stored_pve_token(state_dir: &Path) -> Result<Option<PveToken>, String> {
    let path = secrets_dir(state_dir).join("pve-token");
    match read_optional(&path) {
        Ok(None) => Ok(None),
        Ok(Some(text)) => PveToken::parse(&text)
            .map(Some)
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) => Err(format!("{}: {}", path.display(), error.kind())),
    }
}

fn state_dir(state: &AppState) -> Result<PathBuf, ApiError> {
    state.instance.state_dir.clone().ok_or(ApiError::new(
        ErrorCode::OperationRefused,
        "This instance keeps no secrets: it has no state directory.",
    ))
}

fn stored_error(error: &std::io::Error) -> ApiError {
    tracing::error!(%error, "could not store a secret");
    ApiError::new(ErrorCode::Internal, "The secret could not be stored.")
}

/// Set the Proxmox token: the body is `USER@REALM!TOKENID=SECRET`. The
/// server uses it at once and at every start. Never readable.
#[utoipa::path(
    put,
    path = "/v1/secrets/pve-token",
    tag = "config",
    security(("bearer" = [])),
    request_body(content = String, content_type = "text/plain", description = "USER@REALM!TOKENID=SECRET"),
    responses(
        (status = 204, description = "Stored and in use"),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 409, body = hs_model::ErrorBody, description = "`operation_refused`: the token comes from HOMESTACK_PVE_TOKEN_FILE, or the instance has no state directory"),
        (status = 422, body = hs_model::ErrorBody, description = "Not a Proxmox API token"),
    )
)]
pub async fn put_pve_token(State(state): State<AppState>, body: Bytes) -> Result<StatusCode, ApiError> {
    if state.pve.fixed() {
        return Err(ApiError::new(
            ErrorCode::OperationRefused,
            "The Proxmox token of this instance comes from HOMESTACK_PVE_TOKEN_FILE.",
        ));
    }
    let dir = state_dir(&state)?;
    let text = Secret::new(String::from_utf8(body.to_vec()).unwrap_or_default());
    let token = PveToken::parse(text.expose()).map_err(|_| {
        ApiError::new(
            ErrorCode::ValidationFailed,
            "The body must be a Proxmox API token: USER@REALM!TOKENID=SECRET.",
        )
    })?;
    let line = Secret::new(format!("{}\n", text.expose().trim()));
    let _writing = SECRET_WRITES.lock().await;
    // The client is built before anything is stored, and swapped in only
    // after the token is: a failure leaves the stored and the used token
    // as they were.
    let client = state.pve.prepare(token).map_err(ApiError::from)?;
    write_private(&secrets_dir(&dir).join("pve-token"), line.expose().as_bytes()).map_err(|e| stored_error(&e))?;
    let id = state.pve.install(client);
    tracing::info!(pve_token = id, "Proxmox token replaced");
    Ok(StatusCode::NO_CONTENT)
}

/// Replace the certificate Caddy serves, then make Caddy load it. The key
/// and certificate change together: the new files and copies of the old
/// ones are staged first, and if a rename or Caddy fails, the previous
/// pair is put back.
#[utoipa::path(
    put,
    path = "/v1/secrets/tls",
    tag = "config",
    security(("bearer" = [])),
    request_body = TlsCertificate,
    responses(
        (status = 204, description = "Stored, and loaded by Caddy"),
        (status = 401, body = hs_model::ErrorBody, description = "Missing or invalid bearer token"),
        (status = 409, body = hs_model::ErrorBody, description = "`operation_refused`: the instance has no state directory"),
        (status = 422, body = hs_model::ErrorBody, description = "Not a PEM certificate and key, or Caddy refused them (the previous ones are kept)"),
    )
)]
pub async fn put_tls(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Result<StatusCode, ApiError> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let request: TlsCertificate = if is_json {
        serde_json::from_slice(&body)
            .map_err(|_| ApiError::new(ErrorCode::ValidationFailed, "Invalid request body."))?
    } else {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "The request body must be JSON (Content-Type: application/json).",
        ));
    };
    let pair =
        TlsPair::checked(request.cert, request.key).map_err(|m| ApiError::new(ErrorCode::ValidationFailed, m))?;
    let dir = tls_dir(&state_dir(&state)?);
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let _writing = SECRET_WRITES.lock().await;
    let restore = [
        Restore::new(&key_path).map_err(|e| stored_error(&e))?,
        Restore::new(&cert_path).map_err(|e| stored_error(&e))?,
    ];
    let key = Staged::new(&key_path, pair.key.expose().as_bytes()).map_err(|e| stored_error(&e))?;
    let cert = Staged::new(&cert_path, pair.cert.as_bytes()).map_err(|e| stored_error(&e))?;
    if let Err(error) = key.commit().and_then(|()| cert.commit()) {
        restore_pair(&restore);
        return Err(stored_error(&error));
    }
    let Some(admin) = &state.instance.caddy_admin else {
        tracing::info!("certificate replaced");
        return Ok(StatusCode::NO_CONTENT);
    };
    match reload_caddy(admin).await {
        Ok(()) => {
            tracing::info!("certificate replaced and loaded by Caddy");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(error) => {
            tracing::warn!(
                error,
                "Caddy did not load the new certificate; the previous one is put back"
            );
            restore_pair(&restore);
            if let Err(error) = reload_caddy(admin).await {
                tracing::error!(error, "Caddy did not load the previous certificate either");
            }
            Err(ApiError::new(
                ErrorCode::ValidationFailed,
                "Caddy did not accept the certificate; the previous one is kept.",
            ))
        }
    }
}

/// Make Caddy load its configuration again, which reads the certificate
/// files again: `GET /config/`, then `POST /load` with it and
/// `Cache-Control: must-revalidate` (Caddy skips an unchanged config
/// otherwise).
async fn reload_caddy(admin: &url::Url) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let base = admin.as_str().trim_end_matches('/');
    let config = client
        .get(format!("{base}/config/"))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("reading Caddy's config: {error}"))?
        .bytes()
        .await
        .map_err(|error| format!("reading Caddy's config: {error}"))?;
    let response = client
        .post(format!("{base}/load"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "must-revalidate")
        .body(config)
        .send()
        .await
        .map_err(|error| format!("loading Caddy's config: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(format!("Caddy answered {status}: {}", text.trim()))
    }
}

/// The routes, for `routes::v1`; their body limit is [`MAX_SECRET_BYTES`].
pub fn routes() -> axum::Router<AppState> {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::put;
    axum::Router::new()
        .route(
            "/v1/secrets/pve-token",
            put(put_pve_token).layer(DefaultBodyLimit::max(MAX_SECRET_BYTES)),
        )
        .route(
            "/v1/secrets/tls",
            put(put_tls).layer(DefaultBodyLimit::max(MAX_SECRET_BYTES)),
        )
}
