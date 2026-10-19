//! `deploy.toml`: what `homestack-deploy` deploys where (`docs/migration/deploy.md`).
//!
//! Paths may start with `~/`; relative ones are relative to the file.
//! Secrets stay in their own files: the deploy token, the instances'
//! Proxmox token, the client token, the OVH key.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    /// The node image, `REGISTRY/PATH:TAG`.
    pub image: String,
    /// Instances are `<name>.<domain>`; the certificate is `*.<domain>`.
    pub domain: String,
    /// Storage the image is pulled into (content `vztmpl`).
    pub template_storage: String,
    #[serde(default = "default_pool")]
    pub pool: String,
    pub bridge: String,
    pub cidr: u8,
    pub gateway: Ipv4Addr,
    #[serde(default)]
    pub nameserver: Option<Ipv4Addr>,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    #[serde(default = "default_cores")]
    pub cores: u32,
    #[serde(default = "default_disk")]
    pub disk_gib: u32,
    pub pve: PveSection,
    pub server: ServerSection,
    pub acme: AcmeSection,
    pub instances: Vec<InstanceSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PveSection {
    pub endpoints: Vec<Url>,
    /// `homestack@pve!deploy` (`homestack-pve.sh install`).
    pub deploy_token_file: PathBuf,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
}

/// What every instance runs with.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// The instances' Proxmox token, sent through `PUT /v1/secrets/pve-token`.
    pub pve_token_file: PathBuf,
    /// `HOMESTACK_PVE_POOL`.
    pub pool: String,
    /// `HOMESTACK_PVE_GOLD_POOL`.
    #[serde(default = "default_gold_pool")]
    pub gold_pool: String,
    /// `server.toml`, uploaded to every instance.
    pub config_file: PathBuf,
    /// The client token; the instances get its SHA-256.
    pub api_token_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeSection {
    #[serde(default = "default_directory")]
    pub directory: String,
    /// `OVH_AK`, `OVH_AS`, `OVH_CK` and optionally `OVH_END_POINT`, when
    /// the zone is at OVH.
    #[serde(default)]
    pub ovh_file: Option<PathBuf>,
    /// `SPACESHIP_API_KEY` and `SPACESHIP_API_SECRET`, when the zone is at
    /// Spaceship.
    #[serde(default)]
    pub spaceship_file: Option<PathBuf>,
    /// The public zone that holds `<domain>`.
    pub zone: String,
    /// Where the account and the certificate are kept.
    pub dir: PathBuf,
    /// Extra CA the instances' certificate is checked against (a staging
    /// certificate); the system roots otherwise.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpec {
    /// Host name, also `HOMESTACK_INSTANCE`.
    pub name: String,
    pub node: String,
    pub ctid: u32,
    pub address: Ipv4Addr,
    /// Storage of the container's root disk (content `rootdir`).
    pub storage: String,
}

fn default_pool() -> String {
    "homestack-service".to_owned()
}
fn default_gold_pool() -> String {
    "homestack-gold".to_owned()
}
const fn default_memory() -> u32 {
    256
}
const fn default_cores() -> u32 {
    1
}
const fn default_disk() -> u32 {
    2
}
fn default_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".to_owned()
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

impl DeployConfig {
    /// Read and check `path`; paths in it become absolute.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("{} does not exist; `homestack install` writes it", path.display())
            } else {
                format!("{}: {}", path.display(), e.kind())
            }
        })?;
        let base = path.parent().unwrap_or(Path::new("."));
        let mut config = Self::parse(&text)?;
        config.resolve_paths(base, &home());
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), String> {
        if !self.image.contains(':') || self.image.rsplit(':').next().is_none_or(str::is_empty) {
            return Err(format!("image {:?} needs a tag (REGISTRY/PATH:TAG)", self.image));
        }
        if !self.domain.split('.').all(dns_label) || !self.domain.contains('.') {
            return Err(format!("domain {:?} is not a DNS name", self.domain));
        }
        if !(1..=32).contains(&self.cidr) {
            return Err("cidr must be 1-32".to_owned());
        }
        if self.instances.is_empty() {
            return Err("no [[instances]]".to_owned());
        }
        if self.pve.endpoints.is_empty() {
            return Err("[pve] endpoints is empty".to_owned());
        }
        match (&self.acme.ovh_file, &self.acme.spaceship_file) {
            (Some(_), Some(_)) => return Err("[acme] takes only one of ovh_file and spaceship_file".to_owned()),
            (None, None) => return Err("[acme] needs ovh_file or spaceship_file".to_owned()),
            _ => {}
        }
        if !self.domain.ends_with(&format!(".{}", self.acme.zone)) && self.domain != self.acme.zone {
            return Err(format!("domain {} is not in zone {}", self.domain, self.acme.zone));
        }
        let mut seen = std::collections::BTreeSet::new();
        for instance in &self.instances {
            if !dns_label(&instance.name) {
                return Err(format!("instance name {:?} is not a host name", instance.name));
            }
            if !(100..=999_999_999).contains(&instance.ctid) {
                return Err(format!(
                    "{}: ctid {} is outside 100-999999999",
                    instance.name, instance.ctid
                ));
            }
            for key in [
                instance.name.clone(),
                instance.ctid.to_string(),
                instance.address.to_string(),
            ] {
                if !seen.insert(key.clone()) {
                    return Err(format!("{key} appears twice in [[instances]]"));
                }
            }
        }
        Ok(())
    }

    fn resolve_paths(&mut self, base: &Path, home: &Path) {
        let fix = |path: &mut PathBuf| {
            let text = path.to_string_lossy().into_owned();
            *path = if let Some(rest) = text.strip_prefix("~/") {
                home.join(rest)
            } else if path.is_relative() {
                base.join(&*path)
            } else {
                path.clone()
            };
        };
        fix(&mut self.pve.deploy_token_file);
        if let Some(path) = &mut self.pve.ca_file {
            fix(path);
        }
        fix(&mut self.server.pve_token_file);
        fix(&mut self.server.config_file);
        fix(&mut self.server.api_token_file);
        for file in [&mut self.acme.ovh_file, &mut self.acme.spaceship_file]
            .into_iter()
            .flatten()
        {
            fix(file);
        }
        fix(&mut self.acme.dir);
        if let Some(path) = &mut self.acme.ca_file {
            fix(path);
        }
    }

    /// The instances `only` names, all without it.
    pub fn selected(&self, only: &[String]) -> Result<Vec<&InstanceSpec>, String> {
        if let Some(unknown) = only
            .iter()
            .find(|name| !self.instances.iter().any(|i| &i.name == *name))
        {
            return Err(format!("no instance {unknown} in [[instances]]"));
        }
        Ok(self
            .instances
            .iter()
            .filter(|instance| only.is_empty() || only.contains(&instance.name))
            .collect())
    }

    /// `https://<name>.<domain>`.
    pub fn url(&self, instance: &InstanceSpec) -> Result<Url, String> {
        Url::parse(&format!("https://{}.{}", instance.name, self.domain)).map_err(|error| error.to_string())
    }

    /// What `self` and `other` share: a CTID, an address or a server name.
    /// Two deployments that share any of them would take each other's
    /// container, address or name (owner, 2026-10-14: the live gate's
    /// instances took the address of the installed servers).
    #[must_use]
    pub fn clashes(&self, other: &Self) -> Vec<String> {
        let mut found = Vec::new();
        for mine in &self.instances {
            for theirs in &other.instances {
                if mine.ctid == theirs.ctid {
                    found.push(format!("CT {} ({} and {})", mine.ctid, mine.name, theirs.name));
                }
                if mine.address == theirs.address {
                    found.push(format!("address {} ({} and {})", mine.address, mine.name, theirs.name));
                }
                let (a, b) = (self.url(mine), other.url(theirs));
                if a.is_ok() && a == b {
                    found.push(format!("name {}.{}", mine.name, self.domain));
                }
            }
        }
        found
    }
    /// Replace the image tag (or the full `REGISTRY/PATH:TAG` reference).
    pub fn with_tag(&mut self, tag: &str) -> Result<(), String> {
        if tag.trim().is_empty() || tag.chars().any(char::is_whitespace) {
            return Err(format!("invalid image tag {tag:?}"));
        }
        let image = if tag.contains(':') {
            tag.to_owned()
        } else {
            let (repository, _) = self
                .image
                .rsplit_once(':')
                .ok_or_else(|| format!("image {:?} needs a tag (REGISTRY/PATH:TAG)", self.image))?;
            format!("{repository}:{tag}")
        };
        crate::node::image_file(&image)?;
        self.image = image;
        Ok(())
    }
}

/// Set `image` in `deploy.toml`, keeping the rest of the file as it is.
pub fn save_image(path: &Path, image: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |e: std::io::Error| format!("{}: {}", path.display(), e.kind());
    let text = std::fs::read_to_string(path).map_err(fail)?;
    let mut document: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    match document.get_mut("image").and_then(toml_edit::Item::as_value_mut) {
        // The comment after it stays.
        Some(value) => {
            let decor = value.decor().clone();
            *value = image.into();
            *value.decor_mut() = decor;
        }
        None => document["image"] = toml_edit::value(image),
    }
    let fresh = path.with_extension("toml.new");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&fresh)
        .map_err(fail)?;
    file.write_all(document.to_string().as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    std::fs::rename(&fresh, path).map_err(fail)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const EXAMPLE: &str = r#"
image = "ghcr.io/hipotures/homestack-server:0.1.0-0123456789ab"
domain = "hs.example.net"
template_storage = "local"
bridge = "vmbr0"
cidr = 24
gateway = "192.0.2.1"
[pve]
endpoints = ["https://pve1.example.net:8006"]
deploy_token_file = "~/.config/homestack/pve-token-deploy"
[server]
pve_token_file = "pve-token"
pool = "homestack"
config_file = "/etc/homestack/server.toml"
api_token_file = "api-token"
[acme]
ovh_file = "integrations/ovh-dns"
zone = "example.net"
dir = "acme"
[[instances]]
name = "api1"
node = "pve1"
ctid = 997
address = "192.0.2.97"
storage = "local-zfs"
"#;

    #[test]
    fn an_example_reads_with_its_defaults_and_paths() -> Result<(), String> {
        let mut config = DeployConfig::parse(EXAMPLE)?;
        config.resolve_paths(Path::new("/conf"), Path::new("/home/u"));
        assert_eq!(config.pool, "homestack-service");
        assert_eq!((config.memory_mib, config.cores, config.disk_gib), (256, 1, 2));
        assert_eq!(config.server.gold_pool, "homestack-gold");
        assert_eq!(
            config.pve.deploy_token_file,
            Path::new("/home/u/.config/homestack/pve-token-deploy")
        );
        assert_eq!(config.server.pve_token_file, Path::new("/conf/pve-token"));
        assert_eq!(config.server.config_file, Path::new("/etc/homestack/server.toml"));
        assert_eq!(config.acme.directory, "https://acme-v02.api.letsencrypt.org/directory");
        assert_eq!(
            config.url(&config.instances[0])?.as_str(),
            "https://api1.hs.example.net/"
        );
        assert!(config.selected(&["api2".to_owned()]).is_err());
        Ok(())
    }

    #[test]
    fn mistakes_are_refused() {
        let cases = [
            (
                "0.1.0-0123456789ab\"",
                "0.1.0-0123456789ab\"\nextra = 1",
                "unknown field",
            ),
            ("homestack-server:0.1.0-0123456789ab", "homestack-server", "needs a tag"),
            ("ctid = 997", "ctid = 99", "outside"),
            ("name = \"api1\"", "name = \"API1\"", "host name"),
            ("zone = \"example.net\"", "zone = \"other.net\"", "not in zone"),
        ];
        for (from, to, expected) in cases {
            let text = EXAMPLE.replacen(from, to, 1);
            match DeployConfig::parse(&text) {
                Err(error) => assert!(error.contains(expected), "{expected}: {error}"),
                Ok(_) => panic!("{expected}: accepted"),
            }
        }
        let twice = format!(
            "{EXAMPLE}[[instances]]\nname = \"api2\"\nnode = \"pve2\"\nctid = 997\naddress = \"192.0.2.98\"\nstorage = \"local-lvm\"\n"
        );
        assert!(DeployConfig::parse(&twice).is_err_and(|e| e.contains("997 appears twice")));
    }

    #[test]
    fn the_dns_api_is_ovh_or_spaceship_and_only_one() -> Result<(), String> {
        let ovh = DeployConfig::parse(EXAMPLE)?;
        assert_eq!(ovh.acme.ovh_file.as_deref(), Some(Path::new("integrations/ovh-dns")));
        assert_eq!(ovh.acme.spaceship_file, None);

        let text = EXAMPLE.replacen(
            "ovh_file = \"integrations/ovh-dns\"",
            "spaceship_file = \"integrations/spaceship-dns\"",
            1,
        );
        let mut spaceship = DeployConfig::parse(&text)?;
        spaceship.resolve_paths(Path::new("/conf"), Path::new("/home/u"));
        assert_eq!(spaceship.acme.ovh_file, None);
        assert_eq!(
            spaceship.acme.spaceship_file.as_deref(),
            Some(Path::new("/conf/integrations/spaceship-dns"))
        );

        let both = EXAMPLE.replacen(
            "ovh_file = \"integrations/ovh-dns\"",
            "ovh_file = \"o\"\nspaceship_file = \"s\"",
            1,
        );
        assert!(DeployConfig::parse(&both).is_err_and(|e| e.contains("only one of")));
        let neither = EXAMPLE.replacen("ovh_file = \"integrations/ovh-dns\"\n", "", 1);
        assert!(DeployConfig::parse(&neither).is_err_and(|e| e.contains("ovh_file or spaceship_file")));
        Ok(())
    }

    #[test]
    fn with_tag_replaces_the_tag_or_full_image_and_refuses_bad_tags() -> Result<(), String> {
        let mut config = DeployConfig::parse(EXAMPLE)?;
        config.with_tag("0.1.214-c71ef134237d")?;
        assert_eq!(config.image, "ghcr.io/hipotures/homestack-server:0.1.214-c71ef134237d");
        config.with_tag("registry.example.net/other/server:1.2.3")?;
        assert_eq!(config.image, "registry.example.net/other/server:1.2.3");
        for bad in ["", "   ", "bad/tag", "registry:5000/no-tag", "with space"] {
            assert!(config.with_tag(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn save_image_rewrites_only_the_image_entry() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("deploy.toml");
        std::fs::write(
            &path,
            "# header\nimage = \"r/p:old\" # comment\ndomain = \"hs.example.net\"\n",
        )?;
        save_image(&path, "r/p:new")?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "# header\nimage = \"r/p:new\" # comment\ndomain = \"hs.example.net\"\n"
        );
        Ok(())
    }
}

#[cfg(test)]
mod clash_tests {
    use super::DeployConfig;

    fn config(domain: &str, rows: &[(&str, u32, &str)]) -> DeployConfig {
        let mut text = format!(
            "image = \"i\"\ndomain = \"{domain}\"\ntemplate_storage = \"local\"\nbridge = \"vmbr0\"\ncidr = 24\n\
gateway = \"192.0.2.1\"\n[pve]\nendpoints = [\"https://pve1.example.net:8006\"]\ndeploy_token_file = \"d\"\n\
[server]\npve_token_file = \"p\"\npool = \"homestack\"\nconfig_file = \"/c\"\napi_token_file = \"a\"\n\
[acme]\novh_file = \"o\"\nzone = \"example.net\"\ndir = \"acme\"\n"
        );
        for (name, ctid, address) in rows {
            text.push_str(&format!(
                "[[instances]]\nname = \"{name}\"\nnode = \"pve1\"\nctid = {ctid}\naddress = \"{address}\"\nstorage = \"local-lvm\"\n"
            ));
        }
        toml::from_str(&text).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn a_shared_ctid_address_or_name_is_a_clash_and_nothing_else_is() {
        let installed = config("hs.example.net", &[("api1", 115, "192.0.2.97")]);
        let same_address = config("test.example.net", &[("t1", 997, "192.0.2.97")]);
        let same_name = config("hs.example.net", &[("api1", 997, "192.0.2.94")]);
        let apart = config("test.example.net", &[("t1", 997, "192.0.2.94")]);
        assert_eq!(same_address.clashes(&installed), ["address 192.0.2.97 (t1 and api1)"]);
        assert_eq!(same_name.clashes(&installed), ["name api1.hs.example.net"]);
        assert!(apart.clashes(&installed).is_empty());
    }
}
