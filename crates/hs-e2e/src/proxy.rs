//! Read-only APT access through an explicit QEMU guest forwarding rule.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, Response, StatusCode};
use axum::routing::any;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::Result;

pub struct Proxy {
    pub port: u16,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn upstream(uri: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(uri)?;
    if url.scheme() != "http"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("only public HTTP APT repository URLs are allowed".into());
    }
    let allowed = match url.host_str() {
        Some("deb.debian.org") => url.path().starts_with("/debian/") || url.path().starts_with("/debian-security/"),
        Some("security.debian.org") => url.path().starts_with("/debian-security/"),
        Some("download.proxmox.com") => url.path().starts_with("/debian/pve/"),
        _ => false,
    };
    if !allowed {
        return Err("repository is outside the E2E allowlist".into());
    }
    // Proxmox publishes its signed no-subscription repository over HTTP.
    // Debian supports HTTPS; never disable certificate checks to upgrade PVE.
    if url.host_str() != Some("download.proxmox.com") {
        url.set_scheme("https").map_err(|()| "cannot select HTTPS")?;
    }
    Ok(url)
}

pub async fn start() -> Result<Proxy> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()?;
    let app = axum::Router::new().fallback(any(forward)).with_state(client);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("APT proxy stopped: {error}");
        }
    });
    Ok(Proxy { port, task })
}

async fn forward(State(client): State<reqwest::Client>, request: Request) -> Response<Body> {
    let method = request.method().clone();
    let uri = request.uri().to_string();
    drop(request);
    let result = async move {
        if method != Method::GET && method != Method::HEAD {
            return Err(StatusCode::METHOD_NOT_ALLOWED);
        }
        let url = upstream(&uri).map_err(|_| StatusCode::FORBIDDEN)?;
        let response = client.request(method, url).send().await.map_err(|error| {
            eprintln!("APT upstream request failed: {error}");
            StatusCode::BAD_GATEWAY
        })?;
        // Do not redirect APT to another host or forward client-controlled headers.
        if response.status().is_redirection() {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let mut reply = Response::builder().status(response.status());
        for name in ["content-length", "content-type", "last-modified"] {
            if let Some(value) = response.headers().get(name) {
                reply = reply.header(name, value);
            }
        }
        reply
            .body(Body::from_stream(response.bytes_stream()))
            .map_err(|_| StatusCode::BAD_GATEWAY)
    }
    .await;
    match result {
        Ok(response) => response,
        Err(status) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = status;
            response
        }
    }
}

pub fn unused_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?
        .local_addr()?
        .port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_only_accepts_exact_repository_hosts_and_paths() {
        for (url, scheme) in [
            ("http://deb.debian.org/debian/dists/trixie/InRelease", "https"),
            (
                "http://security.debian.org/debian-security/dists/trixie-security/InRelease",
                "https",
            ),
            ("http://download.proxmox.com/debian/pve/dists/trixie/InRelease", "http"),
        ] {
            assert_eq!(
                upstream(url).map(|u| u.scheme().to_owned()).ok().as_deref(),
                Some(scheme)
            );
        }
        for url in [
            "http://127.0.0.1/debian/",
            "http://169.254.169.254/debian/",
            "http://deb.debian.org.evil.test/debian/",
            "http://deb.debian.org/private/",
            "http://user@deb.debian.org/debian/",
            "http://deb.debian.org:8080/debian/",
            "https://deb.debian.org/debian/",
            "http://deb.debian.org/debian/?target=x",
            "http://download.proxmox.com/debian/pve/../../private",
        ] {
            assert!(upstream(url).is_err(), "accepted {url}");
        }
    }
}
