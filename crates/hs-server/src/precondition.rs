//! The configuration version a state-changing request expects (mapping 8.2).
//!
//! Every request that changes state names the configuration it was planned
//! against in `HomeStack-Config-Version`. An instance with another version
//! refuses it (`409 config_version_mismatch`), so no instance acts on a
//! configuration the client did not see. Handlers take [`ExpectedConfig`]
//! as an extractor; the first state-changing routes arrive in phase 3.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use hs_model::ErrorCode;

use crate::AppState;
use crate::config_store::Stored;
use crate::error::ApiError;

/// Request header with the expected configuration version.
pub const CONFIG_VERSION_HEADER: &str = "homestack-config-version";

/// The stored configuration, once the request's expected version matches.
#[derive(Debug)]
pub struct ExpectedConfig(pub Arc<Stored>);

impl FromRequestParts<AppState> for ExpectedConfig {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        expected_config(&parts.headers, state).map(Self)
    }
}

/// The stored configuration when `headers` name its version.
pub(crate) fn expected_config(headers: &HeaderMap, state: &AppState) -> Result<Arc<Stored>, ApiError> {
    let current = state.config.current().ok_or_else(ApiError::config_missing)?;
    let expected = headers
        .get(CONFIG_VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::PreconditionRequired,
                "State-changing requests must send HomeStack-Config-Version.",
            )
        })?;
    if expected != current.version {
        return Err(ApiError::new(
            ErrorCode::ConfigVersionMismatch,
            "This server has another configuration version than the request expects.",
        ));
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_store::{ConfigStore, Precondition};
    use crate::{ApiToken, PveClient, PveClientConfig};
    use axum::Router;
    use axum::routing::post;
    use hs_pve::PveToken;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SERVER_TOML: &str = "version = 1\n[node]\nname = \"pve1\"\ngold_vmid = 100\n[storage_layouts]\nhomestack-storage-1 = [\"local\"]\n[root]\ndisk = \"scsi0\"\n[home]\ndisk = \"scsi1\"\ndefault_size = \"20G\"\n[network]\nprefix = \"192.0.2\"\ncidr = 24\ngateway = \"192.0.2.1\"\ndns = [\"192.0.2.53\"]\n[cloud_init]\nsnippet_storage = \"local\"\nsnippet_dir = \"/var/lib/vz/snippets\"\n[user]\nname = \"user\"\nuid = 1000\ngid = 1000\n";

    async fn serve(store: ConfigStore) -> Result<String, Box<dyn std::error::Error>> {
        let token = PveToken::parse("homestack@pve!server=secret")?;
        let state = AppState {
            pve_token_id: Arc::from(token.id()),
            pools: Arc::new(crate::Pools::default()),
            pve: Arc::new(PveClient::new(PveClientConfig::new(
                vec!["https://localhost:1".parse()?],
                token,
            ))?),
            api_token: Arc::new(ApiToken::parse(&"t".repeat(32))?),
            config: Arc::new(store),
            ops: Arc::new(crate::operations::OpsState::new(crate::AppOptions::default())),
        };
        let app = Router::new()
            .route(
                "/change",
                post(|ExpectedConfig(config): ExpectedConfig| async move { config.version.clone() }),
            )
            .layer(axum::middleware::from_fn(crate::error::request_context))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(format!("http://{addr}/change"))
    }

    async fn post_with(url: &str, version: Option<&str>) -> Result<(u16, serde_json::Value, String), reqwest::Error> {
        let mut request = reqwest::Client::new().post(url);
        if let Some(version) = version {
            request = request.header(CONFIG_VERSION_HEADER, version);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or_default(), text))
    }

    #[tokio::test]
    async fn state_changes_need_the_current_configuration_version() -> TestResult {
        let dir = tempfile::tempdir()?;
        let empty = ConfigStore::open(dir.path().join("none.toml"))?;
        let (status, body, _) = post_with(&serve(empty).await?, Some("x")).await?;
        assert_eq!((status, body["error"]["code"].as_str()), (409, Some("config_missing")));

        let store = ConfigStore::open(dir.path().join("server.toml"))?;
        let version = store.replace(SERVER_TOML.to_owned(), &Precondition::Absent)?.version;
        let url = serve(store).await?;
        let (status, body, _) = post_with(&url, None).await?;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (428, Some("precondition_required"))
        );
        let (status, body, _) = post_with(&url, Some(&"0".repeat(64))).await?;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (409, Some("config_version_mismatch"))
        );
        let (status, _, text) = post_with(&url, Some(&version)).await?;
        assert_eq!((status, text), (200, version));
        Ok(())
    }
}
