//! Public DNS for Pangolin resources. The public delegation selects the API.
use std::net::IpAddr;
use std::path::Path;

use hs_deploy::dns::{self, DnsHost};
use hs_deploy::ovh::{OvhClient, OvhCredentials};
use hs_deploy::spaceship::{SpaceshipClient, SpaceshipCredentials};

pub struct Record {
    pub zone: String,
    pub sub: String,
    pub target: String,
    pub provider: DnsHost,
    pub exists: bool,
}

pub enum Plan {
    Existing {
        name: String,
        target: String,
        addresses: Vec<IpAddr>,
    },
    Managed(Record),
}

impl Plan {
    pub fn name(&self) -> String {
        match self {
            Self::Existing { name, .. } => name.clone(),
            Self::Managed(record) => record.name(),
        }
    }
    pub fn target(&self) -> &str {
        match self {
            Self::Existing { target, .. } => target,
            Self::Managed(record) => &record.target,
        }
    }
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Existing { name, addresses, .. } => vec![
                format!("Reuse existing public DNS for {name}; no DNS changes"),
                format!(
                    "Addresses: {}",
                    addresses.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
                ),
            ],
            Self::Managed(record) => vec![format!(
                "{} CNAME {} → {} in {}; wait for public DNS before Pangolin",
                if record.exists { "Publish" } else { "Add" },
                record.name(),
                record.target,
                record.provider_name()
            )],
        }
    }
}

impl Record {
    pub fn name(&self) -> String {
        format!("{}.{}", self.sub, self.zone)
    }
    pub fn provider_name(&self) -> &'static str {
        match self.provider {
            DnsHost::Ovh => "OVH",
            DnsHost::Spaceship => "Spaceship",
            DnsHost::Other(_) => unreachable!(),
        }
    }
}

enum Client {
    Ovh(OvhClient),
    Spaceship(SpaceshipClient),
}

async fn client(root: &Path, provider: &DnsHost) -> Result<Client, String> {
    let dir = root.join("integrations");
    match provider {
        DnsHost::Ovh => {
            let path = dir.join("ovh-dns");
            if !path.is_file() {
                return Err("Configure the OVH DNS integration before publishing this domain".into());
            }
            Ok(Client::Ovh(
                OvhClient::connect(OvhCredentials::from_file(&path)?).await?,
            ))
        }
        DnsHost::Spaceship => {
            let path = dir.join("spaceship-dns");
            if !path.is_file() {
                return Err("Configure the Spaceship DNS integration before publishing this domain".into());
            }
            Ok(Client::Spaceship(SpaceshipClient::new(
                SpaceshipCredentials::from_file(&path)?,
            )?))
        }
        DnsHost::Other(host) => Err(format!(
            "Public DNS is served by {host}; only OVH and Spaceship are supported"
        )),
    }
}

async fn existing(client: &Client, record: &Record) -> Result<bool, String> {
    let entries = match client {
        Client::Ovh(api) => api.records(&record.zone, &record.sub).await?,
        Client::Spaceship(api) => {
            let mut entries = Vec::new();
            for entry in api.records(&record.zone).await? {
                let name = entry["name"].as_str().ok_or("Invalid Spaceship record name")?;
                if name.eq_ignore_ascii_case(&record.sub) {
                    entries.push(entry);
                }
            }
            entries
        }
    };
    if entries.is_empty() {
        return Ok(false);
    }
    let matches = entries.len() == 1
        && match client {
            Client::Ovh(_) => {
                entries[0]["fieldType"].as_str() == Some("CNAME")
                    && entries[0]["target"]
                        .as_str()
                        .is_some_and(|value| same_name(value, &record.target))
            }
            Client::Spaceship(_) => {
                entries[0]["type"].as_str() == Some("CNAME")
                    && entries[0]["cname"]
                        .as_str()
                        .is_some_and(|value| same_name(value, &record.target))
            }
        };
    if !matches {
        return Err(format!(
            "{} already has a DNS record that differs from CNAME → {}; inspect it before publishing",
            record.name(),
            record.target
        ));
    }
    Ok(true)
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim_end_matches('.').eq_ignore_ascii_case(b.trim_end_matches('.'))
}

pub async fn review(root: &Path, name: &str, target: &str) -> Result<Plan, String> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let target = target.trim_end_matches('.').to_ascii_lowercase();
    if same_name(&name, &target) || target.parse::<std::net::IpAddr>().is_ok() {
        return Err("The Pangolin dashboard must have a distinct DNS hostname for the CNAME target".into());
    }
    let addresses = dns::public_addresses(&name).await?;
    if !addresses.is_empty() {
        let target_addresses = dns::public_addresses(&target).await?;
        if !addresses.iter().any(|address| target_addresses.contains(address)) {
            return Err(format!(
                "Public DNS for {name} shares no address with the Pangolin server {target}; correct DNS before publishing"
            ));
        }
        return Ok(Plan::Existing {
            name,
            target,
            addresses,
        });
    }
    let (zone, provider) = dns::public_zone(&name).await?;
    let sub = name
        .strip_suffix(&format!(".{zone}"))
        .filter(|sub| !sub.is_empty())
        .ok_or("Cannot create a CNAME at the DNS zone apex")?
        .to_owned();
    let mut record = Record {
        zone,
        sub,
        target,
        provider,
        exists: false,
    };
    let api = client(root, &record.provider).await?;
    record.exists = existing(&api, &record).await?;
    Ok(Plan::Managed(record))
}

pub async fn apply(root: &Path, plan: &Plan) -> Result<String, String> {
    let record = match plan {
        Plan::Existing {
            name,
            target,
            addresses,
        } => {
            let now = dns::public_addresses(name).await?;
            let target_addresses = dns::public_addresses(target).await?;
            if now != *addresses || !now.iter().any(|address| target_addresses.contains(address)) {
                return Err("Existing public DNS changed since review; review again".into());
            }
            return Ok(format!("Existing public DNS for {name} verified; records preserved"));
        }
        Plan::Managed(record) => record,
    };
    let (zone, provider) = dns::public_zone(&record.name()).await?;
    if zone != record.zone || provider != record.provider {
        return Err("Public DNS delegation changed since review; review again".into());
    }
    let api = client(root, &record.provider).await?;
    if existing(&api, record).await? != record.exists {
        return Err("Public DNS records changed since review; review again".into());
    }
    if !record.exists {
        match &api {
            Client::Ovh(api) => {
                api.add_cname(&record.zone, &record.sub, &record.target).await?;
            }
            Client::Spaceship(api) => api.add_cname(&record.zone, &record.sub, &record.target).await?,
        }
    } else if let Client::Ovh(api) = &api {
        // A previous record creation may have succeeded before its refresh failed.
        api.refresh(&record.zone).await?;
    }
    dns::wait_for_public_cname(&record.name(), &record.target).await?;
    Ok(format!(
        "{} → {} verified in public DNS ({})",
        record.name(),
        record.target,
        record.provider_name()
    ))
}
