//! TLS pass-through to the two public GHCR hosts needed by Proxmox's OCI pull.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, Response, StatusCode};
use axum::routing::any;
use tokio::net::{TcpListener, TcpStream};

use crate::{Result, proxy::Proxy};

fn host(authority: &str) -> Result<&str> {
    match authority {
        "ghcr.io:443" => Ok("ghcr.io"),
        "pkg-containers.githubusercontent.com:443" => Ok("pkg-containers.githubusercontent.com"),
        _ => Err("registry tunnel destination is outside the E2E allowlist".into()),
    }
}

fn public(ip: IpAddr) -> bool {
    let IpAddr::V4(ip) = ip else { return false };
    let [a, b, _, _] = ip.octets();
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_documentation()
        && !ip.is_broadcast()
        && !ip.is_multicast()
        && a != 0
        && a < 224
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 198 && (18..=19).contains(&b))
        && !(a == 192 && b == 0)
}

pub async fn start() -> Result<Proxy> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, axum::Router::new().fallback(any(tunnel))).await {
            eprintln!("Registry proxy stopped: {error}");
        }
    });
    Ok(Proxy { port, task })
}

async fn tunnel(mut request: Request) -> Response<Body> {
    let result = async {
        if request.method() != Method::CONNECT {
            return Err(StatusCode::METHOD_NOT_ALLOWED);
        }
        let destination = host(&request.uri().to_string())
            .map_err(|_| StatusCode::FORBIDDEN)?
            .to_owned();
        // Resolve once and connect to the checked IP, preventing a second DNS
        // lookup from redirecting an allowlisted name to a LAN service.
        let addresses = tokio::net::lookup_host((destination.as_str(), 443))
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let mut upstream = None;
        for address in addresses.filter(|address| public(address.ip())) {
            if let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(address)).await {
                upstream = Some(stream);
                break;
            }
        }
        let mut upstream = upstream.ok_or(StatusCode::BAD_GATEWAY)?;
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            if let Ok(stream) = upgrade.await {
                let mut stream = hyper_util::rt::TokioIo::new(stream);
                let _ = tokio::time::timeout(
                    Duration::from_secs(900),
                    tokio::io::copy_bidirectional(&mut stream, &mut upstream),
                )
                .await;
            }
        });
        Ok::<_, StatusCode>(Response::new(Body::empty()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tunnels_cannot_reach_lan_or_arbitrary_hosts() -> Result<()> {
        for name in ["ghcr.io:443", "pkg-containers.githubusercontent.com:443"] {
            assert!(host(name).is_ok());
        }
        for name in [
            "ghcr.io:80",
            "127.0.0.1:443",
            "ghcr.io.evil.test:443",
            "user@ghcr.io:443",
            "10.77.0.1:443",
        ] {
            assert!(host(name).is_err());
        }
        for ip in [
            "127.0.0.1",
            "10.77.0.1",
            "192.168.100.1",
            "169.254.169.254",
            "100.64.1.1",
            "198.18.1.1",
            "224.0.0.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public(ip.parse()?), "accepted {ip}");
        }
        assert!(public("140.82.114.33".parse()?));
        Ok(())
    }
}
