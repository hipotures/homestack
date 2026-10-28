//! Secrets sent through the API (mapping 8.1, `phase-11.md`): write-only
//! routes that keep them in the instance's state directory, mode `0600`.
//!
//! - `secrets/pve-token`: the Proxmox token, used at once and at every
//!   start (unless `HOMESTACK_PVE_TOKEN_FILE` names one).
//! - `tls/current`: selects the immutable certificate generation Caddy serves. The
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
    /// Parse the chain and private key and prove their public keys match.
    pub fn checked(cert: String, key: Secret) -> Result<Self, &'static str> {
        let pair = Self { cert, key };
        pair.validate()?;
        Ok(pair)
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
        let certs = CertificateDer::pem_slice_iter(self.cert.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "The certificate must be a PEM certificate chain.")?;
        for cert in &certs {
            let (remaining, _) = x509_parser::parse_x509_certificate(cert.as_ref())
                .map_err(|_| "The certificate chain contains an invalid certificate.")?;
            if !remaining.is_empty() {
                return Err("The certificate chain contains an invalid certificate.");
            }
        }
        let key = PrivateKeyDer::from_pem_slice(self.key.expose().as_bytes())
            .map_err(|_| "The key must be a PEM private key.")?;
        rustls::sign::CertifiedKey::from_der(certs, key, &rustls::crypto::aws_lc_rs::default_provider())
            .and_then(|key| key.keys_match())
            .map_err(|_| "The certificate and private key must form a valid matching pair.")
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

fn read_optional(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reconcile and validate TLS before the HTTP listener (and therefore Caddy)
/// starts. A stored valid generation takes precedence over bootstrap input.
pub fn store_bootstrap_tls(state_dir: &Path, pair: Option<&TlsPair>) -> Result<(), String> {
    crate::tls_store::bootstrap(state_dir, pair)
        .map_err(|error| format!("could not prepare persistent TLS: {}", error.kind()))
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
/// and certificate are an immutable generation selected by one durable rename.
/// If Caddy refuses it, the prior generation is atomically selected again.
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
    let _writing = SECRET_WRITES.lock().await;
    let rotation = crate::tls_store::rotate(&dir, &pair).map_err(|e| stored_error(&e))?;
    let Some(admin) = &state.instance.caddy_admin else {
        tracing::info!("certificate replaced");
        return Ok(StatusCode::NO_CONTENT);
    };
    match reload_caddy(admin, &rotation.active).await {
        Ok(()) => {
            tracing::info!("certificate replaced and loaded by Caddy");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(error) => {
            tracing::warn!(
                error,
                "Caddy did not load the new certificate; the previous one is put back"
            );
            crate::tls_store::select(&dir, rotation.previous.as_ref()).map_err(|e| stored_error(&e))?;
            if let Some(previous) = &rotation.previous
                && let Err(error) = reload_caddy(admin, previous).await
            {
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
async fn reload_caddy(admin: &url::Url, generation: &crate::tls_store::Generation) -> Result<(), String> {
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
    // Pin both files to this immutable generation. Never ask Caddy to resolve
    // `current` twice: it could change between the key and certificate reads.
    let mut config: serde_json::Value =
        serde_json::from_slice(&config).map_err(|_| "Caddy returned an invalid configuration".to_owned())?;
    let files = config
        .pointer_mut("/apps/tls/certificates/load_files")
        .and_then(serde_json::Value::as_array_mut)
        .filter(|files| files.len() == 1)
        .ok_or("Caddy has no single managed TLS pair")?;
    files[0]["certificate"] = serde_json::json!(generation.path("cert.pem"));
    files[0]["key"] = serde_json::json!(generation.path("key.pem"));
    let response = client
        .post(format!("{base}/load"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "must-revalidate")
        .json(&config)
        .send()
        .await
        .map_err(|error| format!("loading Caddy's config: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        Err(format!("Caddy answered {status}"))
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
