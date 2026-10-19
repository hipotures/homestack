//! The servers of `homestack install` (phase 13c, section 8 of
//! `docs/migration/phase-13c.md`): after the Proxmox part, one server per
//! node, made through the Proxmox API with the deploy token
//! (`hs-deploy`), then `client.toml`, and `homestack status` at the end.
//!
//! The form supplies the workspace configuration and domain. Instances come
//! from the cluster, their addresses from this desktop's DNS, and endpoints
//! from the nodes' API certificates. What is
//! missing is a problem shown in the confirmation, before any change.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use hs_deploy::config::DeployConfig;
use hs_deploy::dns::DnsHost;
use hs_install::Plan;

/// The node image's repository; the tag is a numeric API release.
pub const IMAGE_REPOSITORY: &str = "ghcr.io/hipotures/homestack-server";
/// The pool of the server containers.
pub const SERVICE_POOL: &str = "homestack-service";

/// A deployment in progress.
pub type Deploying<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// What the installer does outside Proxmox's root shell: this desktop's DNS,
/// the deployment, and how the client reaches the servers.
pub trait Servers: Send + Sync {
    /// The IPv4 address `name` resolves to on this desktop.
    fn resolve(&self, name: &str) -> Option<Ipv4Addr>;
    /// The public zone that holds `domain` and who serves its DNS.
    fn public_zone(&self, domain: &str) -> Result<(String, DnsHost), String>;
    /// The certificate (obtained when the stored one will not do) and every
    /// instance of `config`.
    fn deploy<'a>(&'a self, config: &'a DeployConfig, say: &'a (dyn Fn(&str) + Send + Sync)) -> Deploying<'a>;
    /// `[server] urls` and `ca_file` of `client.toml`.
    fn client_server(&self, config: &DeployConfig) -> (Vec<String>, Option<String>);
}

/// The real ones: the system resolver, `hs_deploy`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealServers;

impl Servers for RealServers {
    fn resolve(&self, name: &str) -> Option<Ipv4Addr> {
        (name, 443)
            .to_socket_addrs()
            .ok()?
            .find_map(|address| match address.ip() {
                std::net::IpAddr::V4(ip) => Some(ip),
                std::net::IpAddr::V6(_) => None,
            })
    }

    fn public_zone(&self, domain: &str) -> Result<(String, DnsHost), String> {
        // The form plans again after every answer: ask once per domain.
        static FOUND: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, (String, DnsHost)>>> =
            std::sync::OnceLock::new();
        let found = FOUND.get_or_init(Default::default);
        if let Some(known) = found.lock().ok().and_then(|map| map.get(domain).cloned()) {
            return Ok(known);
        }
        let handle = tokio::runtime::Handle::try_current().map_err(|error| error.to_string())?;
        let answer = tokio::task::block_in_place(|| handle.block_on(hs_deploy::dns::public_zone(domain)))?;
        if let Ok(mut map) = found.lock() {
            map.insert(domain.to_owned(), answer.clone());
        }
        Ok(answer)
    }

    fn deploy<'a>(&'a self, config: &'a DeployConfig, say: &'a (dyn Fn(&str) + Send + Sync)) -> Deploying<'a> {
        Box::pin(async move {
            let say = |line: &str| say(line);
            hs_deploy::deploy::cert(config, false, false, &say).await?;
            hs_deploy::deploy::apply(config, &[], &say).await
        })
    }

    fn client_server(&self, config: &DeployConfig) -> (Vec<String>, Option<String>) {
        let urls = config
            .instances
            .iter()
            .filter_map(|instance| config.url(instance).ok())
            .map(|url| url.as_str().trim_end_matches('/').to_owned())
            .collect();
        (
            urls,
            config.acme.ca_file.as_ref().map(|path| path.display().to_string()),
        )
    }
}

/// The most recent API release reachable from this build's source revision.
#[must_use]
pub fn default_image() -> Option<String> {
    let version = option_env!("HOMESTACK_API_VERSION")?;
    Some(format!("{IMAGE_REPOSITORY}:{version}"))
}

/// What the servers part of an install will do.
#[derive(Debug, Clone)]
pub struct ServerPlan {
    /// The deployment, once nothing is missing.
    pub config: Option<DeployConfig>,
    /// `deploy.toml`, kept for `homestack-deploy status` and `cert --push`.
    pub deploy_toml: String,
    /// The server configuration, uploaded to every instance.
    pub server_toml: String,
    /// Confirmed installation settings, also used to generate `client.toml`.
    pub install_config: String,
    /// One line per instance and what else is done, for the confirmation.
    pub lines: Vec<String>,
    /// What keeps the install from starting.
    pub problems: Vec<String>,
    /// What does not stop it.
    pub warnings: Vec<String>,
}

/// Where the desktop's files are.
#[derive(Debug, Clone)]
pub struct Files {
    /// The configuration directory (`~/.config/homestack`); the token files
    /// are there too.
    pub dir: PathBuf,
    pub home: PathBuf,
}

impl Files {
    #[must_use]
    pub fn install_config(&self) -> PathBuf {
        self.dir.join("config.toml")
    }
    #[must_use]
    pub fn server_toml(&self) -> PathBuf {
        self.dir.join("server.toml")
    }
    #[must_use]
    pub fn deploy_toml(&self) -> PathBuf {
        self.dir.join("deploy.toml")
    }
    #[must_use]
    pub fn api_token(&self) -> PathBuf {
        self.dir.join("api-token")
    }
    #[must_use]
    pub fn ovh(&self) -> PathBuf {
        self.dir.join("ovh-dns")
    }
    #[must_use]
    pub fn spaceship(&self) -> PathBuf {
        self.dir.join("spaceship-dns")
    }
    #[must_use]
    pub fn acme(&self) -> PathBuf {
        self.dir.join("acme")
    }
    fn short(&self, path: &Path) -> String {
        path.strip_prefix(&self.home)
            .map_or_else(|_| path.display().to_string(), |rest| format!("~/{}", rest.display()))
    }
}

fn dns_label(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && !text.starts_with('-')
        && !text.ends_with('-')
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn quoted(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

/// Why `address` is not on the gateway's network, if it is not.
fn outside_network(address: Ipv4Addr, network: &hs_model::config::Network) -> Option<String> {
    let Ok(gateway) = network.gateway.parse::<Ipv4Addr>() else {
        return Some(format!("but the gateway {:?} is not an IPv4 address", network.gateway));
    };
    let mask = u32::MAX.checked_shl(32 - u32::from(network.cidr)).unwrap_or(0);
    let base = Ipv4Addr::from(u32::from(gateway) & mask);
    (u32::from(address) & mask != u32::from(base))
        .then(|| format!("outside the network {base}/{} of the gateway {gateway}", network.cidr))
}

/// `[storage_layouts]`: node `...N` uses `homestack-storage-N`, made of the
/// chosen workspace-disk storages that are on that node.
fn layouts(plan: &Plan, warnings: &mut Vec<String>) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut layouts = std::collections::BTreeMap::new();
    for node in &plan.cluster.nodes {
        let Ok(name) = hs_model::node::NodeName::parse(node)
            .map_err(|_| ())
            .and_then(|parsed| hs_model::config::active_layout_name(&parsed).map_err(|_| ()))
        else {
            warnings.push(format!(
                "{node}: its name does not end in a number, so it gets no workspace disks"
            ));
            continue;
        };
        let storages: Vec<String> = plan
            .settings
            .storages
            .iter()
            .filter(|id| {
                plan.cluster
                    .storages
                    .get(*id)
                    .is_some_and(|(_, nodes)| nodes.is_empty() || nodes.contains(node))
            })
            .cloned()
            .collect();
        if storages.is_empty() {
            warnings.push(format!("{node}: no chosen workspace-disk storage there"));
            continue;
        }
        layouts.insert(name, storages);
    }
    layouts
}

/// The servers of `plan` under `domain`.
pub fn plan_servers(
    plan: &Plan,
    domain: &str,
    image: Option<&str>,
    files: &Files,
    servers: &dyn Servers,
    input: Result<String, String>,
) -> ServerPlan {
    let mut out = ServerPlan {
        config: None,
        deploy_toml: String::new(),
        server_toml: String::new(),
        install_config: String::new(),
        lines: Vec::new(),
        problems: Vec::new(),
        warnings: Vec::new(),
    };
    let domain = domain.trim().trim_end_matches('.').to_lowercase();
    if domain.is_empty() {
        out.problems
            .push("Server domain is not set: the servers are named apiN.<domain>".to_owned());
        return out;
    }
    if !domain.contains('.') || !domain.split('.').all(dns_label) {
        out.problems.push(format!("Server domain {domain:?} is not a DNS name"));
        return out;
    }
    // The public zone of the domain and its DNS API: the certificate's
    // DNS-01 record is written there.
    let (zone, dns_key) = match servers.public_zone(&domain) {
        Ok((zone, DnsHost::Ovh)) => (zone, Some(("OVH", files.ovh(), "OVH_AK, OVH_AS, OVH_CK"))),
        Ok((zone, DnsHost::Spaceship)) => (
            zone,
            Some((
                "Spaceship",
                files.spaceship(),
                "SPACESHIP_API_KEY, SPACESHIP_API_SECRET",
            )),
        ),
        Ok((zone, DnsHost::Other(name_server))) => {
            out.problems.push(format!(
                "the DNS of {zone} is at {name_server}: HomeStack gets the certificate for *.{domain} only through OVH or Spaceship"
            ));
            (zone, None)
        }
        Err(error) => {
            out.problems
                .push(format!("cannot tell which public zone holds {domain}: {error}"));
            (String::new(), None)
        }
    };

    let mut network = None;
    match input.and_then(|text| prepare_input(plan, &text, &mut out.warnings)) {
        Ok((server, text)) => {
            network = Some(server.network.clone());
            out.server_toml = server.to_toml();
            out.install_config = text;
        }
        Err(error) => out.problems.push(format!("Installation settings: {error}")),
    }

    let image = image.map(str::to_owned).or_else(default_image);
    if image.is_none() {
        out.problems.push(
            "this build has no API release tag: fetch release tags and rebuild, or give --server-image".to_owned(),
        );
    }
    let settings = &plan.settings;
    let Some(template) = settings.template_storage.clone() else {
        out.problems.push("no storage for the server image".to_owned());
        return out;
    };
    if plan.cluster.bridge.is_empty() {
        out.problems
            .push("The server network bridge is not selected".to_owned());
    }

    // The instances: one per online node with a server-disk storage, each
    // numbered by its address (`hs_model::address`).
    let mut instances = Vec::new();
    for (index, node) in plan.cluster.nodes.iter().enumerate() {
        let storage = settings.service_storages.iter().find(|id| {
            plan.cluster
                .storages
                .get(*id)
                .is_some_and(|(_, nodes)| nodes.is_empty() || nodes.contains(node))
        });
        let Some(storage) = storage else {
            out.warnings
                .push(format!("{node}: no chosen server-disk storage there, so no server"));
            continue;
        };
        let name = format!("api{}", index + 1);
        let fqdn = format!("{name}.{domain}");
        let Some(address) = servers.resolve(&fqdn) else {
            out.problems.push(format!(
                "{fqdn} does not resolve on this desktop: give it the server's address in DNS"
            ));
            continue;
        };
        if let Some(network) = &network
            && let Some(outside) = outside_network(address, network)
        {
            out.problems.push(format!("{fqdn} is {address}, {outside}"));
        }
        let ctid = hs_model::address::vmid_of(address);
        if ctid < 100 {
            out.problems.push(format!(
                "{fqdn} is {address}, which gives CT {ctid}; Proxmox numbers start at 100"
            ));
            continue;
        }
        // Adoption has already validated the guest's HomeStack server tag.
        // Deployment follows the pool changes in this same confirmed plan.
        let is_server = |id: u32, kind: &str, pool: &str| {
            kind == "lxc" && (pool == SERVICE_POOL || pool.is_empty() && settings.adopt.contains(&id))
        };
        let mut replaced = "";
        for (id, (guest, kind, pool)) in &plan.cluster.guests {
            if *id == ctid && *guest == name && is_server(*id, kind, pool) {
                replaced = "  (replaced)";
            } else if *id == ctid {
                out.problems.push(format!("CT {ctid} for {fqdn} is taken by {guest}"));
            } else if *guest == name && is_server(*id, kind, pool) {
                out.problems.push(format!(
                    "{name} is CT {id}, but its address gives CT {ctid}: remove CT {id} first \
                     (homestack-deploy --config <deploy.toml> remove --only {name} --yes)"
                ));
            }
        }
        out.lines
            .push(format!("{fqdn}  {address}  {node}  CT {ctid}  {storage}{replaced}"));
        instances.push((name, node.clone(), ctid, address, storage.clone()));
    }
    if instances.is_empty() && out.problems.is_empty() {
        out.problems.push("no node can hold a server".to_owned());
    }
    let endpoints: Vec<String> = plan
        .cluster
        .nodes
        .iter()
        .filter_map(|node| plan.cluster.api_names.get(node))
        .map(|name| format!("https://{name}:8006"))
        .collect();
    if endpoints.is_empty() {
        out.problems
            .push("no node's API certificate names it, so the servers cannot reach Proxmox".to_owned());
    }

    // The certificate: the stored one while it is good, otherwise from
    // Let's Encrypt with the DNS API key of the zone.
    let stored = hs_deploy::acme::stored(&files.acme()).ok().flatten();
    match (stored, &dns_key) {
        // The stored one, still good: nothing changes.
        (Some(stored), _) if hs_deploy::acme::current(&stored, &domain, now()) => {}
        (_, Some((host, file, _))) if file.exists() => out.lines.push(format!(
            "certificate *.{domain} from Let's Encrypt (DNS at {host}, zone {zone})"
        )),
        (_, Some((host, file, keys))) => out.problems.push(format!(
            "a certificate for *.{domain} needs the {host} API key in {} ({keys})",
            files.short(file)
        )),
        (_, None) => {}
    }
    let Some(network) = network else {
        return out;
    };
    let Some(image) = image else {
        return out;
    };
    out.lines.insert(0, format!("image {image}"));
    if !out.problems.is_empty() {
        return out;
    }

    let mut text = format!(
        "# Made by `homestack install`; `homestack-deploy --config <this file> status` reads it.\n\
image = {}\ndomain = {}\ntemplate_storage = {}\npool = {}\nbridge = {}\ncidr = {}\ngateway = {}\n",
        quoted(&image),
        quoted(&domain),
        quoted(&template),
        quoted(SERVICE_POOL),
        quoted(&plan.cluster.bridge),
        network.cidr,
        quoted(&network.gateway),
    );
    if let Some(dns) = network.dns.first() {
        text.push_str(&format!("nameserver = {}\n", quoted(dns)));
    }
    let path = |file: PathBuf| quoted(&file.display().to_string());
    text.push_str(&format!(
        "\n[pve]\nendpoints = [{}]\ndeploy_token_file = {}\n",
        endpoints.iter().map(|e| quoted(e)).collect::<Vec<_>>().join(", "),
        path(settings.token_dir.join("pve-token-deploy")),
    ));
    text.push_str(&format!(
        "\n[server]\npve_token_file = {}\npool = \"homestack\"\nconfig_file = {}\napi_token_file = {}\n",
        path(settings.token_dir.join("pve-token")),
        path(files.server_toml()),
        path(files.api_token()),
    ));
    let Some((host, dns_file, _)) = &dns_key else {
        return out;
    };
    let key = if *host == "Spaceship" {
        "spaceship_file"
    } else {
        "ovh_file"
    };
    text.push_str(&format!(
        "\n[acme]\n{key} = {}\nzone = {}\ndir = {}\n",
        path(dns_file.clone()),
        quoted(&zone),
        path(files.acme()),
    ));
    for (name, node, ctid, address, storage) in &instances {
        text.push_str(&format!(
            "\n[[instances]]\nname = {}\nnode = {}\nctid = {ctid}\naddress = {}\nstorage = {}\n",
            quoted(name),
            quoted(node),
            quoted(&address.to_string()),
            quoted(storage),
        ));
    }
    match DeployConfig::parse(&text) {
        Ok(config) => {
            out.config = Some(config);
            out.deploy_toml = text;
        }
        Err(error) => out.problems.push(format!("the deployment: {error}")),
    }
    out
}

fn prepare_input(
    plan: &Plan,
    text: &str,
    warnings: &mut Vec<String>,
) -> Result<(hs_model::config::Config, String), String> {
    use toml::{Table, Value};
    let mut document: Table = text
        .parse()
        .map_err(|_| "invalid installation configuration".to_owned())?;
    document.insert("version".into(), Value::Integer(1));
    document.remove("transport");
    document.remove("control");
    let node = plan
        .cluster
        .gold_node
        .as_ref()
        .or_else(|| plan.cluster.nodes.first())
        .ok_or("No online node for the server configuration")?;
    let mut node_table = Table::new();
    node_table.insert("name".into(), Value::String(node.clone()));
    if let Some(gold) = plan.cluster.gold {
        node_table.insert("gold_vmid".into(), Value::Integer(i64::from(gold)));
    }
    document.insert("node".into(), Value::Table(node_table));
    document.insert(
        "storage_layouts".into(),
        Value::Table(
            layouts(plan, warnings)
                .into_iter()
                .map(|(name, ids)| (name, Value::Array(ids.into_iter().map(Value::String).collect())))
                .collect(),
        ),
    );
    let snippets = document
        .entry("cloud_init")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .ok_or("Invalid Cloud-Init settings")?;
    snippets.insert(
        "snippet_storage".into(),
        Value::String(plan.settings.snippet_storage.clone().ok_or("Choose snippet storage")?),
    );
    let server = hs_model::config::Config::from_sections(&document).map_err(|error| error.to_string())?;
    let size = server.default_home_size.trim().to_ascii_uppercase();
    let number = size
        .get(..size.len().saturating_sub(1))
        .filter(|number| !number.starts_with('0') && number.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|number| number.parse::<u64>().ok())
        .filter(|number| *number > 0);
    let factor = match size.as_bytes().last() {
        Some(b'M') => Some(1),
        Some(b'G') => Some(1024),
        Some(b'T') => Some(1024 * 1024),
        _ => None,
    };
    if number
        .zip(factor)
        .and_then(|(number, factor)| number.checked_mul(factor))
        .is_none()
    {
        return Err("Default home size must be a positive integer with M, G or T, for example 20G".to_owned());
    }
    if server.network.prefix.parse::<std::net::Ipv4Addr>().is_ok()
        || format!("{}.0", server.network.prefix)
            .parse::<std::net::Ipv4Addr>()
            .is_err()
    {
        return Err("Workspace IP prefix must contain three IPv4 octets".to_owned());
    }
    if server.network.gateway.parse::<std::net::Ipv4Addr>().is_err()
        || server
            .network
            .dns
            .iter()
            .any(|dns| dns.parse::<std::net::IpAddr>().is_err())
    {
        return Err("Enter valid network gateway and DNS addresses".to_owned());
    }
    let ssh = document
        .entry("workspace_ssh")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .ok_or("Invalid workspace SSH settings")?;
    ssh.insert("user".into(), Value::String(server.user.name.clone()));
    ssh.entry("identities_only").or_insert(Value::Boolean(true));
    ssh.entry("log_level").or_insert(Value::String("FATAL".to_owned()));
    hs_config::client::ClientConfig::from_sections(&document).map_err(|error| error.to_string())?;
    let text = toml::to_string(&document).map_err(|error| error.to_string())?;
    Ok((server, text))
}

/// `client.toml`: the confirmed client settings and deployed server endpoints.
pub fn client_toml(
    install_config: &str,
    urls: &[String],
    ca_file: Option<String>,
    token_file: &str,
) -> Result<String, String> {
    let document: toml::Table = install_config
        .parse()
        .map_err(|_| "invalid installation configuration".to_owned())?;
    let mut client = hs_config::client::ClientConfig::from_sections(&document).map_err(|error| error.to_string())?;
    client.server = Some(hs_config::client::Server {
        urls: urls.to_vec(),
        token_file: token_file.to_owned(),
        ca_file,
    });
    let text = client
        .to_toml()
        .map_err(|_| "[setup] and [sounds] cannot be written back as TOML".to_owned())?;
    hs_config::client::ClientConfig::parse(&text).map_err(|error| error.to_string())?;
    Ok(text)
}

/// Write `text` to `path` (mode 0600); a different file there is kept as
/// `NAME.replaced-<seconds>`. `Ok(Some(kept))` names the kept file.
pub fn write_private(path: &Path, text: &str) -> Result<Option<PathBuf>, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |e: std::io::Error| format!("{}: {}", path.display(), e.kind());
    let mut kept = None;
    match std::fs::read_to_string(path) {
        Ok(old) if old == text => return Ok(None),
        Ok(_) => {
            let seconds = now();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let aside = path.with_file_name(format!("{name}.replaced-{seconds}"));
            std::fs::rename(path, &aside).map_err(fail)?;
            kept = Some(aside);
        }
        Err(_) => {}
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(fail)?;
    file.write_all(text.as_bytes()).map_err(fail)?;
    Ok(kept)
}

/// The client token: the file's, or a new one (32 random bytes, hex).
pub fn ensure_api_token(path: &Path) -> Result<bool, String> {
    use std::io::Read;
    if path.exists() {
        return Ok(false);
    }
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| format!("/dev/urandom: {}", e.kind()))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(path, &format!("{token}\n"))?;
    Ok(true)
}
