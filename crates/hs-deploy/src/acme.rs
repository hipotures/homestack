//! The wildcard certificate of the instances, `*.<domain>`, from an ACME CA
//! (Let's Encrypt) by DNS-01 in the public zone at OVH or Spaceship (owner's
//! decisions, phase 11 and 2026-10-14). Kept on the desktop in `[acme] dir`:
//! `account.json`, `cert.pem`, `key.pem` (mode `0600`).

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use hickory_resolver::Resolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};

use crate::dns::DnsApi;

/// Renew when fewer days than this are left.
pub const RENEW_DAYS: i64 = 30;

/// The stored certificate.
pub struct Stored {
    pub cert: String,
    pub key: String,
    /// Seconds since the Unix epoch.
    pub not_after: i64,
    pub names: Vec<String>,
}

fn paths(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (dir.join("account.json"), dir.join("cert.pem"), dir.join("key.pem"))
}

/// `notAfter` and the DNS names of the first certificate of a PEM chain.
pub fn inspect(cert_pem: &str) -> Result<(i64, Vec<String>), String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).map_err(|e| format!("PEM: {e}"))?;
    let cert = pem.parse_x509().map_err(|e| format!("certificate: {e}"))?;
    let names = cert
        .subject_alternative_name()
        .map_err(|e| format!("certificate: {e}"))?
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    x509_parser::extensions::GeneralName::DNSName(dns) => Some((*dns).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((cert.validity().not_after.timestamp(), names))
}

/// The certificate in `dir`, if both files are there.
pub fn stored(dir: &Path) -> Result<Option<Stored>, String> {
    let (_, cert_path, key_path) = paths(dir);
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {}", path.display(), error.kind())),
    };
    let (Some(cert), Some(key)) = (read(&cert_path)?, read(&key_path)?) else {
        return Ok(None);
    };
    let (not_after, names) = inspect(&cert)?;
    Ok(Some(Stored {
        cert,
        key,
        not_after,
        names,
    }))
}

/// Whether `stored` covers `*.<domain>` for at least [`RENEW_DAYS`] from
/// `now` (seconds since the epoch).
#[must_use]
pub fn current(stored: &Stored, domain: &str, now: i64) -> bool {
    stored.names.iter().any(|name| *name == format!("*.{domain}")) && stored.not_after - now >= RENEW_DAYS * 86_400
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {}", path.display(), e.kind());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(fail)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(fail)?;
    file.write_all(text.as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    std::fs::rename(&temporary, path).map_err(fail)
}

/// Load or create an account in a dedicated private ACME directory.
pub async fn account(directory: &str, dir: &Path) -> Result<Account, String> {
    let (account_path, _, _) = paths(dir);
    let builder = Account::builder().map_err(|e| e.to_string())?;
    if let Ok(text) = std::fs::read_to_string(&account_path) {
        let credentials: AccountCredentials =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", account_path.display()))?;
        return builder.from_credentials(credentials).await.map_err(|e| e.to_string());
    }
    let (account, credentials) = builder
        .create(
            &NewAccount {
                contact: &[],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory.to_owned(),
            None,
        )
        .await
        .map_err(|e| format!("ACME account: {e}"))?;
    let text = serde_json::to_string(&credentials).map_err(|e| e.to_string())?;
    write_private(&account_path, &text)?;
    Ok(account)
}

/// Wait until every authoritative name server of `zone` answers `name`
/// with a TXT record holding `value` (the local DNS may hold another zone
/// of the same name, so the public servers are asked directly).
pub async fn wait_for_txt(
    dns: &DnsApi,
    zone: &str,
    name: &str,
    value: &str,
    say: &impl Fn(&str),
) -> Result<(), String> {
    let mut servers = Vec::new();
    for host in dns.name_servers(zone).await? {
        let addresses = tokio::net::lookup_host((host.as_str(), 53))
            .await
            .map_err(|e| format!("{host}: {e}"))?;
        servers.extend(addresses.map(|a: SocketAddr| a.ip()).filter(IpAddr::is_ipv4));
    }
    if servers.is_empty() {
        return Err(format!("no name server of {zone} could be resolved"));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    for server in servers {
        let mut builder = Resolver::builder_with_config(
            ResolverConfig::from_name_servers(vec![NameServerConfig::udp_and_tcp(server)]),
            TokioRuntimeProvider::default(),
        );
        builder.options_mut().cache_size = 0;
        let resolver = builder.build().map_err(|e| e.to_string())?;
        loop {
            let found = resolver.txt_lookup(format!("{name}.")).await.is_ok_and(|lookup| {
                lookup
                    .answers()
                    .iter()
                    .any(|record| record.data.to_string().contains(value))
            });
            if found {
                say(&format!("  {name} TXT is on {server}"));
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("{name} TXT did not appear on {server} within 10 minutes"));
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    Ok(())
}

/// Get `*.<domain>` and store it in `dir`.
pub async fn obtain(
    directory: &str,
    dir: &Path,
    domain: &str,
    dns: &DnsApi,
    zone: &str,
    say: impl Fn(&str),
) -> Result<Stored, String> {
    let account = account(directory, dir).await?;
    let mut order = account
        .new_order(&NewOrder::new(&[Identifier::Dns(format!("*.{domain}"))]))
        .await
        .map_err(|e| format!("ACME order: {e}"))?;
    let challenge_name = format!("_acme-challenge.{domain}");
    let sub = challenge_name
        .strip_suffix(&format!(".{zone}"))
        .ok_or_else(|| format!("{challenge_name} is not in zone {zone}"))?
        .to_owned();
    let mut records = Vec::new();
    let mut outcome: Result<(), String> = Ok(());
    {
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = match result {
                Ok(authz) => authz,
                Err(e) => {
                    outcome = Err(format!("ACME authorization: {e}"));
                    break;
                }
            };
            match authz.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                status => {
                    outcome = Err(format!("ACME authorization is {status:?}"));
                    break;
                }
            }
            let Some(mut challenge) = authz.challenge(ChallengeType::Dns01) else {
                outcome = Err("the CA offers no dns-01 challenge".to_owned());
                break;
            };
            let value = challenge.key_authorization().dns_value();
            say(&format!("  TXT {challenge_name} at {}", dns.name()));
            match dns.add_txt(zone, &sub, &value).await {
                Ok(id) => records.push(id),
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
            if let Err(e) = wait_for_txt(dns, zone, &challenge_name, &value, &say).await {
                outcome = Err(e);
                break;
            }
            if let Err(e) = challenge.set_ready().await {
                outcome = Err(format!("ACME challenge: {e}"));
                break;
            }
        }
    }
    let result = match outcome {
        Ok(()) => finish(&mut order, dir).await,
        Err(error) => Err(error),
    };
    for record in records {
        if let Err(error) = dns.remove(zone, &record).await {
            say(&format!("  warning: the TXT record {record} was not removed: {error}"));
        }
    }
    result
}

/// Finalize a ready order and save its certificate and private key.
pub async fn finish(order: &mut instant_acme::Order, dir: &Path) -> Result<Stored, String> {
    let policy = RetryPolicy::new()
        .timeout(Duration::from_secs(300))
        .initial_delay(Duration::from_secs(2));
    let status = order
        .poll_ready(&policy)
        .await
        .map_err(|e| format!("ACME order: {e}"))?;
    if status != OrderStatus::Ready {
        return Err(format!("ACME order is {status:?}, not ready"));
    }
    let key = order.finalize().await.map_err(|e| format!("ACME finalize: {e}"))?;
    let cert = order
        .poll_certificate(&policy)
        .await
        .map_err(|e| format!("ACME certificate: {e}"))?;
    let (_, cert_path, key_path) = paths(dir);
    write_private(&key_path, &key)?;
    write_private(&cert_path, &cert)?;
    let (not_after, names) = inspect(&cert)?;
    Ok(Stored {
        cert,
        key,
        not_after,
        names,
    })
}
