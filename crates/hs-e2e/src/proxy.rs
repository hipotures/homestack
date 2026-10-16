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

const ATTEMPTS: u32 = 3;

pub struct Proxy {
    pub port: u16,
    pub(crate) task: JoinHandle<()>,
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

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        // Keep the attempt limit explicit, including transport failures.
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()?)
}

pub async fn start() -> Result<Proxy> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let app = axum::Router::new().fallback(any(forward)).with_state(client()?);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("APT proxy stopped: {error}");
        }
    });
    Ok(Proxy { port, task })
}

/// APT must not lose an entire installation to a single failed upstream request.
/// Only the GET/HEAD requests validated by `forward` reach this function.
async fn fetch(
    client: &reqwest::Client,
    method: Method,
    url: reqwest::Url,
) -> std::result::Result<reqwest::Response, StatusCode> {
    for attempt in 1..=ATTEMPTS {
        let failure = match client.request(method.clone(), url.clone()).send().await {
            Ok(response) => {
                if !matches!(response.status().as_u16(), 408 | 429 | 500 | 502 | 503 | 504) {
                    return Ok(response);
                }
                let failure = format!("HTTP {}", response.status());
                if attempt == ATTEMPTS {
                    eprintln!("\nAPT upstream failed after {ATTEMPTS} attempts: {url}: {failure}");
                    return Ok(response);
                }
                failure
            }
            Err(error) => {
                let failure = crate::describe(&error);
                if attempt == ATTEMPTS {
                    eprintln!("\nAPT upstream failed after {ATTEMPTS} attempts: {url}: {failure}");
                    return Err(StatusCode::BAD_GATEWAY);
                }
                failure
            }
        };
        let delay = Duration::from_secs(u64::from(attempt));
        eprintln!(
            "\nAPT upstream attempt {attempt}/{ATTEMPTS} failed: {url}: {failure}; retrying in {}s",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
    }
    Err(StatusCode::BAD_GATEWAY)
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
        let response = fetch(&client, method, url).await?;
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

pub fn unused_port(address: Ipv4Addr) -> Result<u16> {
    Ok(std::net::TcpListener::bind(SocketAddr::from((address, 0)))?
        .local_addr()?
        .port())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn repository(responses: Vec<Option<StatusCode>>) -> Result<(Proxy, reqwest::Url, Arc<AtomicUsize>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let index = counted.fetch_add(1, Ordering::SeqCst).min(responses.len() - 1);
                let Some(status) = responses[index] else {
                    // Drop the connection before any HTTP response, like an upstream reset.
                    continue;
                };
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let Ok(length) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if length == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..length]);
                }
                let body = if request.starts_with(b"HEAD ") { "" } else { "package" };
                let reply = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: 7\r\nConnection: close\r\n\r\n{body}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("error")
                );
                if stream.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        Ok((
            Proxy { port, task },
            format!("http://127.0.0.1:{port}/package.deb").parse()?,
            requests,
        ))
    }

    #[tokio::test]
    async fn retries_connection_failures_and_temporary_http_errors_for_get_and_head() -> Result<()> {
        let client = client()?;
        for method in [Method::GET, Method::HEAD] {
            let (_server, url, requests) =
                repository(vec![None, Some(StatusCode::SERVICE_UNAVAILABLE), Some(StatusCode::OK)]).await?;
            let response = fetch(&client, method.clone(), url)
                .await
                .map_err(|status| status.to_string())?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.bytes().await?.as_ref(),
                if method == Method::HEAD {
                    &b""[..]
                } else {
                    &b"package"[..]
                }
            );
            assert_eq!(requests.load(Ordering::SeqCst), ATTEMPTS as usize);
        }
        Ok(())
    }

    #[tokio::test]
    async fn persistent_failures_stop_at_the_attempt_limit() -> Result<()> {
        let client = client()?;
        let (_server, url, requests) = repository(vec![None]).await?;
        assert_eq!(
            fetch(&client, Method::GET, url).await.err(),
            Some(StatusCode::BAD_GATEWAY)
        );
        assert_eq!(requests.load(Ordering::SeqCst), ATTEMPTS as usize);
        let (_server, url, requests) = repository(vec![Some(StatusCode::BAD_GATEWAY)]).await?;
        let response = fetch(&client, Method::GET, url)
            .await
            .map_err(|status| status.to_string())?;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(requests.load(Ordering::SeqCst), ATTEMPTS as usize);
        Ok(())
    }

    #[tokio::test]
    async fn permanent_http_errors_and_redirects_are_not_retried() -> Result<()> {
        let client = client()?;
        for status in [
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_IMPLEMENTED,
            StatusCode::FOUND,
        ] {
            let (_server, url, requests) = repository(vec![Some(status)]).await?;
            let response = fetch(&client, Method::GET, url)
                .await
                .map_err(|status| status.to_string())?;
            assert_eq!(response.status(), status);
            assert_eq!(requests.load(Ordering::SeqCst), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejected_destinations_and_methods_never_reach_the_retry_loop() -> Result<()> {
        let client = client()?;
        for (method, url, status) in [
            (
                Method::POST,
                "http://download.proxmox.com/debian/pve/package.deb",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                Method::CONNECT,
                "download.proxmox.com:443",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                Method::GET,
                "http://127.0.0.1/debian/package.deb",
                StatusCode::FORBIDDEN,
            ),
            (
                Method::GET,
                "http://download.proxmox.com/private/package.deb",
                StatusCode::FORBIDDEN,
            ),
        ] {
            let request = Request::builder().method(method).uri(url).body(Body::empty())?;
            assert_eq!(forward(State(client.clone()), request).await.status(), status);
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "Requires access to the official Proxmox APT repository"]
    async fn official_repository_is_reachable_through_the_proxy() -> Result<()> {
        let proxy = start().await?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::http(format!("http://127.0.0.1:{}", proxy.port))?)
            .timeout(Duration::from_secs(60))
            .build()?;
        let response = client
            .get("http://download.proxmox.com/debian/pve/dists/trixie/InRelease")
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        assert!(response.starts_with(b"-----BEGIN PGP SIGNED MESSAGE-----"));
        Ok(())
    }

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
