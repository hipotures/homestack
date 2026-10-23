//! A short script in a container's own shell through the Proxmox console
//! (`termproxy` and its `vncwebsocket`): the deploy token's `VM.Console`
//! reaches the shell that console mode `shell` opens inside the container
//! (the container's root, not the node's), over the API, without SSH. What
//! the script carries is never put into the container's configuration, a
//! command line on the node, a log or a message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hs_core::pve::Pve;
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::config::DeployConfig;

/// Run `script` in CT `ctid`'s console shell; what it printed, up to the
/// first line that contains `until` (after the shell's prompt, say), which must
/// appear within `limit`. The script must print `until` without writing it
/// literally (e.g. `"MARK""ER"`), since the terminal may echo its input.
pub async fn run(
    pve: &impl Pve,
    config: &DeployConfig,
    node: &str,
    ctid: u32,
    script: &str,
    until: &str,
    limit: Duration,
) -> Result<String, String> {
    let fail = |what: &str| format!("CT {ctid} console: {what}");
    let proxy = pve
        .post(&format!("/nodes/{node}/lxc/{ctid}/termproxy"), &[])
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let port = proxy["port"]
        .as_u64()
        .or_else(|| proxy["port"].as_str().and_then(|port| port.parse().ok()))
        .ok_or_else(|| fail("no port"))?;
    let ticket = proxy["ticket"].as_str().ok_or_else(|| fail("no ticket"))?;
    let user = proxy["user"].as_str().ok_or_else(|| fail("no user"))?;
    let endpoint = config.pve.endpoints.first().ok_or("[pve] endpoints is empty")?;
    let host = endpoint
        .host_str()
        .ok_or_else(|| fail("endpoint without host"))?
        .to_owned();
    let mut url = endpoint.clone();
    url.set_scheme("wss").map_err(|()| fail("endpoint is not HTTPS"))?;
    url.set_path(&format!("/api2/json/nodes/{node}/lxc/{ctid}/vncwebsocket"));
    url.query_pairs_mut()
        .clear()
        .append_pair("port", &port.to_string())
        .append_pair("vncticket", ticket);
    let token = hs_pve::PveToken::from_file(&config.pve.deploy_token_file).map_err(|e| e.to_string())?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| fail("invalid request"))?;
    let mut authorization = HeaderValue::from_str(&token.header_value()).map_err(|_| fail("invalid token"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("Authorization", authorization);

    let tcp = tokio::net::TcpStream::connect((host.as_str(), endpoint.port_or_known_default().unwrap_or(8006)))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config(config)?))
        .connect(
            ServerName::try_from(host.clone()).map_err(|_| fail("invalid host name"))?,
            tcp,
        )
        .await
        .map_err(|e| fail(&format!("TLS: {e}")))?;
    let (mut socket, _) = tokio_tungstenite::client_async(request, tls)
        .await
        .map_err(|e| fail(&e.to_string()))?;

    let started = Instant::now();
    let next = |started: Instant| limit.checked_sub(started.elapsed()).filter(|left| !left.is_zero());
    socket
        .send(Message::text(format!("{user}:{ticket}\n")))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let mut output = String::new();
    // The proxy confirms the ticket with "OK" before the shell's own output.
    loop {
        let left = next(started).ok_or_else(|| fail("no answer"))?;
        match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(message))) => {
                output.push_str(&String::from_utf8_lossy(&message.into_data()));
                if output.starts_with("OK") {
                    output.clear();
                    break;
                }
            }
            Ok(Some(Err(e))) => return Err(fail(&e.to_string())),
            Ok(None) => return Err(fail("closed")),
            Err(_) => return Err(fail("no answer")),
        }
    }
    // Input sent before the shell runs can be lost: wait for its root prompt.
    socket
        .send(Message::text("0:1:\n"))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    while !output.contains("# ") {
        let left = next(started).ok_or_else(|| fail("no shell prompt"))?;
        match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(message))) => output.push_str(&String::from_utf8_lossy(&message.into_data())),
            Ok(Some(Err(e))) => return Err(fail(&e.to_string())),
            Ok(None) | Err(_) => return Err(fail("no shell prompt")),
        }
    }
    output.clear();
    for line in script.split_inclusive('\n') {
        socket
            .send(Message::text(format!("0:{}:{line}", line.len())))
            .await
            .map_err(|e| fail(&e.to_string()))?;
    }
    loop {
        if let Some(at) = output.lines().position(|line| line.contains(until)) {
            let _ = socket.close(None).await;
            return Ok(output.lines().take(at + 1).collect::<Vec<_>>().join("\n"));
        }
        let left = next(started).ok_or_else(|| fail("the script did not finish"))?;
        match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(message))) => output.push_str(&String::from_utf8_lossy(&message.into_data())),
            Ok(Some(Err(e))) => return Err(fail(&e.to_string())),
            Ok(None) | Err(_) => return Err(fail("the script did not finish")),
        }
    }
}

/// The system roots and the optional `[pve] ca_file`, as for the API.
fn tls_config(config: &DeployConfig) -> Result<rustls::ClientConfig, String> {
    let roots = match &config.pve.ca_file {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
            CertificateDer::pem_slice_iter(&pem)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| format!("{}: invalid certificates", path.display()))?
        }
        None => Vec::new(),
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(roots, Arc::clone(&provider))
        .map_err(|_| "Cannot load certificate trust roots".to_owned())?;
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}
