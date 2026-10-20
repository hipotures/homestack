//! The public DNS of the certificate's zone: which zone holds a domain and
//! who serves it (its NS records, asked of public resolvers, since the
//! local DNS may hold a zone of the same name), and the API that writes the
//! DNS-01 record there (OVH or Spaceship).

use std::net::{IpAddr, Ipv4Addr};

use hickory_resolver::Resolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;

use crate::config::AcmeSection;
use crate::ovh::{OvhClient, OvhCredentials};
use crate::spaceship::{SpaceshipClient, SpaceshipCredentials};

/// Public resolvers, asked instead of the local DNS.
const PUBLIC: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
];

/// Who serves a public zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsHost {
    Ovh,
    Spaceship,
    /// Neither; the first name server.
    Other(String),
}

/// The host of a zone by its name servers.
#[must_use]
pub fn host_of(name_servers: &[String]) -> DnsHost {
    let all = |suffix: &str| {
        !name_servers.is_empty()
            && name_servers
                .iter()
                .all(|name| name.trim_end_matches('.').ends_with(suffix))
    };
    if all(".ovh.net") {
        DnsHost::Ovh
    } else if all(".spaceship.net") {
        DnsHost::Spaceship
    } else {
        DnsHost::Other(
            name_servers
                .first()
                .map_or_else(|| "nowhere".to_owned(), |name| name.trim_end_matches('.').to_owned()),
        )
    }
}

/// `domain` and its parents down to two labels, the longest first.
#[must_use]
pub fn zone_candidates(domain: &str) -> Vec<String> {
    let labels: Vec<&str> = domain.trim_end_matches('.').split('.').collect();
    (0..labels.len().saturating_sub(1))
        .map(|start| labels[start..].join("."))
        .collect()
}

fn public_resolver(server: IpAddr) -> Result<Resolver<TokioRuntimeProvider>, String> {
    let mut builder = Resolver::builder_with_config(
        ResolverConfig::from_name_servers(vec![NameServerConfig::udp_and_tcp(server)]),
        TokioRuntimeProvider::default(),
    );
    builder.options_mut().cache_size = 0;
    builder.build().map_err(|e| e.to_string())
}

/// The name servers of `zone` in the public DNS; empty when `zone` is not
/// a zone there.
pub async fn public_name_servers(zone: &str) -> Result<Vec<String>, String> {
    let mut last = String::new();
    for server in PUBLIC {
        let resolver = public_resolver(server)?;
        match resolver.ns_lookup(format!("{zone}.")).await {
            Ok(lookup) => {
                return Ok(lookup
                    .answers()
                    .iter()
                    .map(|record| record.data.to_string().trim_end_matches('.').to_owned())
                    .collect());
            }
            Err(error) if error.is_no_records_found() || error.is_nx_domain() => return Ok(Vec::new()),
            Err(error) => last = format!("{server}: {error}"),
        }
    }
    Err(format!("NS of {zone}: {last}"))
}

/// The public zone that holds `domain` and who serves it.
pub async fn public_zone(domain: &str) -> Result<(String, DnsHost), String> {
    for zone in zone_candidates(domain) {
        let name_servers = public_name_servers(&zone).await?;
        if !name_servers.is_empty() {
            let host = host_of(&name_servers);
            return Ok((zone, host));
        }
    }
    Err(format!("no public zone holds {domain}"))
}

/// The API that writes the DNS-01 record.
pub enum DnsApi {
    Ovh(OvhClient),
    Spaceship(SpaceshipClient),
}

/// A record [`DnsApi::add_txt`] made.
pub enum TxtRecord {
    Ovh(u64),
    Spaceship { sub: String, value: String },
}

impl DnsApi {
    /// The API whose key `[acme]` names.
    pub async fn connect(acme: &AcmeSection) -> Result<Self, String> {
        match (&acme.ovh_file, &acme.spaceship_file) {
            (Some(file), None) => Ok(Self::Ovh(OvhClient::connect(OvhCredentials::from_file(file)?).await?)),
            (None, Some(file)) => Ok(Self::Spaceship(SpaceshipClient::new(SpaceshipCredentials::from_file(
                file,
            )?)?)),
            _ => Err("[acme] needs one of ovh_file and spaceship_file".to_owned()),
        }
    }

    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ovh(_) => "OVH",
            Self::Spaceship(_) => "Spaceship",
        }
    }

    pub async fn add_txt(&self, zone: &str, sub: &str, value: &str) -> Result<TxtRecord, String> {
        match self {
            Self::Ovh(ovh) => ovh.add_txt(zone, sub, value).await.map(TxtRecord::Ovh),
            Self::Spaceship(spaceship) => {
                spaceship.add_txt(zone, sub, value).await?;
                Ok(TxtRecord::Spaceship {
                    sub: sub.to_owned(),
                    value: value.to_owned(),
                })
            }
        }
    }

    pub async fn remove(&self, zone: &str, record: &TxtRecord) -> Result<(), String> {
        match (self, record) {
            (Self::Ovh(ovh), TxtRecord::Ovh(id)) => ovh.remove(zone, *id).await,
            (Self::Spaceship(spaceship), TxtRecord::Spaceship { sub, value }) => {
                spaceship.remove_txt(zone, sub, value).await
            }
            _ => Err("the record is not one of this DNS API".to_owned()),
        }
    }

    /// The authoritative name servers of `zone`.
    pub async fn name_servers(&self, zone: &str) -> Result<Vec<String>, String> {
        match self {
            Self::Ovh(ovh) => ovh.name_servers(zone).await,
            Self::Spaceship(_) => public_name_servers(zone).await,
        }
    }
}

impl std::fmt::Display for TxtRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ovh(id) => write!(f, "{id}"),
            Self::Spaceship { sub, .. } => write!(f, "{sub}"),
        }
    }
}
