//! Publish a service through Pangolin's public and optional private HTTP resources.
//! Paired updates preserve existing public routing and policies exactly; only
//! missing private exposure is added. Public-only routing retains its existing workflow.
//! This reads Proxmox and changes only Pangolin routing through Step 7.

mod private;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use hs_core::pve::Pve;
use serde_json::{Map, Value, json};

use super::install::Setting;
use super::pangolin_key::{Client, Credential, Probe};
use super::pangolin_sites::{Context, managed::Pangolin};
use crate::install_flow::Answer;
use crate::services::{
    deploy::{NewInstance, Source},
    instance::{self, Instance},
    manifest::Healthcheck,
};

/// The title of the Pangolin setting.
pub(super) const ACTION: &str = "Pangolin";
pub(super) const DOMAIN: &str = "Pangolin domain";
pub(super) const SUBDOMAIN: &str = "Pangolin subdomain";
const EXPOSURE: &str = "Pangolin exposure";
const PRIVATE_SITES: &str = "Private Sites";

fn paired_default() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Publication {
    domain: String,
    subdomain: String,
    hostname: String,
    local_domain: String,
    #[serde(default = "paired_default")]
    paired: bool,
    #[serde(default)]
    private_sites: Vec<String>,
}

fn check_subdomain(value: &str) -> Result<(), String> {
    if crate::services::manifest::valid_subdomain(value.trim()) {
        Ok(())
    } else {
        Err("A public DNS label: a-z, 0-9 and inner -, up to 63 characters".into())
    }
}

pub(super) fn selected_name(settings: &[Setting], answers: &[Answer]) -> Result<Publication, String> {
    let text = |title: &str| {
        settings
            .iter()
            .position(|s| s.title == title)
            .and_then(|i| match answers.get(i) {
                Some(Answer::Text(value)) => Some(value.trim().to_owned()),
                _ => None,
            })
            .ok_or_else(|| format!("Set {title} before publishing"))
    };
    let subdomain = text(SUBDOMAIN)?;
    check_subdomain(&subdomain)?;
    Ok(Publication {
        domain: selected_domain(settings, answers).into(),
        subdomain,
        hostname: text("Hostname")?,
        local_domain: text("Domain")?,
        paired: settings
            .iter()
            .position(|s| s.title == EXPOSURE)
            .is_none_or(|i| matches!(answers.get(i), Some(Answer::Picked(v)) if v.as_slice() == [0])),
        private_sites: settings
            .iter()
            .position(|s| s.title == PRIVATE_SITES)
            .and_then(|i| match answers.get(i) {
                Some(Answer::Picked(v)) => Some(
                    v.iter()
                        .filter_map(|j| settings[i].choices.get(*j).map(|c| c.0.clone()))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default(),
    })
}

fn publication_path(root: &Path, source: &Source, hostname: &str, domain: &str) -> PathBuf {
    let mut identity = Sha256::new();
    for value in [&source.repository, &source.manifest.id, hostname, domain] {
        identity.update((value.len() as u64).to_be_bytes());
        identity.update(value.as_bytes());
    }
    let hash: String = identity.finalize().iter().map(|b| format!("{b:02x}")).collect();
    root.join("publications").join(format!("{hash}.json"))
}

fn load_publication(root: &Path, source: &Source, hostname: &str, domain: &str) -> Result<Option<Publication>, String> {
    let path = publication_path(root, source, hostname, domain);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Cannot read the saved public hostname".into()),
    };
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect the saved public hostname")?;
    if !meta.is_file()
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.permissions().mode() & 0o077 != 0
        || meta.len() > 4096
    {
        return Err("Saved public hostname must be a small owner-only file".into());
    }
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read the saved public hostname")?;
    let saved: Publication = serde_json::from_slice(&bytes).map_err(|_| "Invalid saved public hostname")?;
    if saved.hostname != hostname || saved.local_domain != domain {
        return Err("Saved public hostname belongs to another deployment".into());
    }
    check_subdomain(&saved.subdomain)?;
    Ok(Some(saved))
}

pub(super) fn restore_publication(
    root: &Path,
    source: &Source,
    hostname: &str,
    domain: &str,
    settings: &mut [Setting],
) -> Result<(), String> {
    let Some(saved) = load_publication(root, source, hostname, domain)? else {
        return Ok(());
    };
    for setting in settings {
        if setting.title == EXPOSURE {
            setting.picked = vec![usize::from(!saved.paired)];
        }
        if setting.title == PRIVATE_SITES {
            setting.choices = saved
                .private_sites
                .iter()
                .map(|n| (n.clone(), "Saved Site; verify when attaching".into()))
                .collect();
            setting.picked = (0..setting.choices.len()).collect();
        }
        if setting.title == SUBDOMAIN {
            setting.text = Some(saved.subdomain.clone());
        }
        if setting.title == DOMAIN {
            let index = match setting.choices.iter().position(|(name, _)| name == &saved.domain) {
                Some(index) => index,
                None => {
                    setting
                        .choices
                        .push((saved.domain.clone(), "Saved domain; verify when attaching".into()));
                    setting.choices.len() - 1
                }
            };
            setting.picked = vec![index];
        }
    }
    Ok(())
}

pub(super) fn remember(root: &Path, source: &Source, plan: &Plan) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let path = publication_path(root, source, &plan.publication.hostname, &plan.publication.local_domain);
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        let parent = path.parent().ok_or("Invalid public hostname path")?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let meta = std::fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() || meta.permissions().mode() & 0o077 != 0
        {
            return Err("Public hostname directory must be private".into());
        }
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(&plan.publication)?)?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    write().map_err(|_| "Published successfully, but cannot save the public hostname locally".into())
}
const PAGE: usize = 100;

#[derive(Clone, PartialEq)]
struct Resource {
    id: u64,
    domain: String,
    name: String,
}

#[derive(Clone, PartialEq)]
struct Site {
    id: u64,
    name: String,
    nice: String,
    online: bool,
}

struct Domain {
    id: String,
    base: String,
}

struct Catalog {
    resources: Vec<Resource>,
    sites: Vec<Site>,
    domains: Vec<Domain>,
}

/// The HomeStack container a resource routes to.
struct Container {
    ctid: u32,
    node: String,
    /// The existing container's reviewed identity; `None` for an instance
    /// this deployment creates before attaching.
    existing: Option<Instance>,
}

pub(super) struct Plan {
    /// `None`: a service without resources, reached at `address` only.
    container: Option<Container>,
    address: String,
    repository: String,
    service: String,
    /// The resource's full domain.
    domain: String,
    domain_id: String,
    base_domain: String,
    publication: Publication,
    /// The existing resource; `None` when Apply creates it from `create`.
    resource: Option<Resource>,
    create: Option<Value>,
    site: Site,
    previous: Option<Target>,
    body: Value,
    bypass_paths: Vec<String>,
    bypass: Bypass,
    /// A paired update never normalizes an existing public resource.
    keep_public: bool,
    public_snapshot: Option<Value>,
    public_receipt: PathBuf,
    private: Option<private::Plan>,
}

/// What the resource lacks of the manifest's login bypass: path rules to add
/// (from `priority` on) and whether its rule evaluation is still off.
#[derive(Default, PartialEq)]
struct Bypass {
    missing: Vec<String>,
    enable: bool,
    priority: i64,
}

#[derive(PartialEq)]
struct Target {
    id: u64,
    body: Value,
}

pub(super) fn requested(settings: &[Setting], answers: &[Answer]) -> bool {
    settings
        .iter()
        .position(|setting| setting.title == ACTION)
        .is_some_and(|at| matches!(answers.get(at), Some(Answer::Picked(indices)) if indices.as_slice() == [1]))
}

pub(super) fn selected_domain<'a>(settings: &'a [Setting], answers: &[Answer]) -> &'a str {
    let Some((index, setting)) = settings.iter().enumerate().find(|(_, s)| s.title == DOMAIN) else {
        return "";
    };
    let Some(Answer::Picked(indices)) = answers.get(index) else {
        return "";
    };
    indices
        .first()
        .and_then(|i| setting.choices.get(*i))
        .map_or("", |(name, _)| name)
}

pub(super) fn settings(source: &Source) -> Vec<Setting> {
    let Some(exposure) = source.manifest.pangolin.as_ref().filter(|_| !source.is_site()) else {
        return Vec::new();
    };
    let action = Setting {
        title: ACTION.to_owned(),
        note: format!(
            "Enter attaches: it connects to Pangolin and reads its verified domains; once attached, Enter \
             refreshes them. Attached, the deployment reuses or creates the Pangolin HTTP resource on the chosen public subdomain and \
             the selected Pangolin domain, routes it through an online Site to one instance (the selected \
             one, or the one this deployment creates) on its {port} port and monitors its health check. \
             Without attaching, no Pangolin request is made.",
            port = exposure.port,
        ),
        choices: vec![
            ("attach".to_owned(), String::new()),
            ("refresh".to_owned(), String::new()),
        ],
        text: None,
        list: false,
        check: None,
        application_input: None,
        auto_text: false,
        hint: None,
        multiple: false,
        picked: vec![0],
    };
    let mut domain = action.clone();
    domain.title = DOMAIN.to_owned();
    domain.note = "Public base domain verified in Pangolin. Independent of the container's local Domain. Enter attaches Pangolin first if needed.".to_owned();
    domain.choices.clear();
    domain.picked.clear();
    let mut subdomain = action.clone();
    subdomain.title = SUBDOMAIN.into();
    subdomain.note = "Public hostname prefix, such as limits. Together with Pangolin domain it forms the public HTTPS address. Independent of the container's Hostname.".into();
    subdomain.choices.clear();
    subdomain.picked.clear();
    subdomain.text = Some(exposure.subdomain.clone());
    subdomain.check = Some(check_subdomain);
    let mut exposure = action.clone();
    exposure.title = EXPOSURE.into();
    exposure.note = "Paired uses one HTTPS hostname: public Pangolin login and private Pangolin Client access through the existing Admin role. Public-only preserves external API integrations. Existing resources are never deleted by this selection.".into();
    exposure.choices = vec![
        ("Public + Private".into(), String::new()),
        ("Public only".into(), String::new()),
    ];
    let mut sites = action.clone();
    sites.title = PRIVATE_SITES.into();
    sites.note = "No selection: retain an existing pair's Sites, or use the public backend's Site. Select additional Sites only when each can reach the same destination. Selecting a Site confirms that network route; HomeStack validates its organization, identity and online state. Attach/refresh Pangolin to load Sites.".into();
    sites.choices.clear();
    sites.picked.clear();
    sites.multiple = true;
    vec![action, subdomain, domain, exposure, sites]
}

/// Load verified domains with the same organization-scoped key used for routing.
pub(super) fn domains(pangolin: &Pangolin, stop: &AtomicBool) -> Result<Vec<String>, String> {
    let domains = Api::open(pangolin, stop)?.domains()?;
    if domains.is_empty() {
        return Err("Pangolin has no verified domain; add one in Pangolin first".to_owned());
    }
    Ok(domains.into_iter().map(|d| d.base).collect())
}

/// Populate the existing multi-select with current Site identifiers.
pub(super) fn sites(pangolin: &Pangolin, stop: &AtomicBool) -> Result<Vec<(String, String)>, String> {
    Ok(Api::open(pangolin, stop)?
        .catalog()?
        .sites
        .into_iter()
        .filter(|s| s.online)
        .map(|s| (s.nice, s.name))
        .collect())
}

pub(super) fn set_sites(settings: &mut [Setting], sites: Vec<(String, String)>) {
    if let Some(setting) = settings.iter_mut().find(|s| s.title == PRIVATE_SITES) {
        let selected: Vec<String> = setting
            .picked
            .iter()
            .filter_map(|i| setting.choices.get(*i).map(|c| c.0.clone()))
            .collect();
        setting.choices = sites;
        for name in &selected {
            if !setting.choices.iter().any(|c| &c.0 == name) {
                setting
                    .choices
                    .push((name.clone(), "Unavailable Site; review will block".into()));
            }
        }
        setting.picked = setting
            .choices
            .iter()
            .enumerate()
            .filter_map(|(i, c)| selected.contains(&c.0).then_some(i))
            .collect();
    }
}

/// Mark Pangolin attached: the deployment then publishes through it.
pub(super) fn attach(settings: &mut [Setting]) {
    if let Some(setting) = settings.iter_mut().find(|setting| setting.title == ACTION) {
        setting.picked = vec![1];
    }
}

/// Whether Pangolin is attached and its domains are loaded.
pub(super) fn attached(settings: &[Setting]) -> bool {
    settings
        .iter()
        .any(|setting| setting.title == ACTION && setting.picked.as_slice() == [1])
        && settings
            .iter()
            .any(|setting| setting.title == DOMAIN && !setting.choices.is_empty())
}

/// Replace the selector's choices, retaining a still-valid selection.
pub(super) fn set_domains(settings: &mut [Setting], domains: Vec<String>) {
    let Some(setting) = settings.iter_mut().find(|setting| setting.title == DOMAIN) else {
        return;
    };
    let selected = setting
        .picked
        .first()
        .and_then(|i| setting.choices.get(*i))
        .map(|(value, _)| value.clone());
    setting.choices = domains.into_iter().map(|name| (name, String::new())).collect();
    setting.picked = match selected {
        Some(value) => {
            let index = match setting.choices.iter().position(|(name, _)| name == &value) {
                Some(index) => index,
                None => {
                    setting
                        .choices
                        .push((value, "Previously selected; no longer verified".into()));
                    setting.choices.len() - 1
                }
            };
            vec![index]
        }
        None if setting.choices.len() == 1 => vec![0],
        None => Vec::new(),
    };
}

struct Api<'a> {
    ctx: &'a Context,
    key: Credential,
    stop: &'a AtomicBool,
}

impl<'a> Api<'a> {
    fn open(pangolin: &'a Pangolin, stop: &'a AtomicBool) -> Result<Self, String> {
        let (ctx, key) = pangolin.api_context()?;
        Ok(Self { ctx, key, stop })
    }

    fn client(&self) -> Client<'_> {
        Client {
            endpoint: &self.ctx.endpoint,
            credential: &self.key,
            stop: self.stop,
            started: Instant::now(),
        }
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        match self
            .client()
            .get(path, Probe::Organization)
            .map_err(|e| e.message().to_owned())?
        {
            (200, Some(data)) => Ok(data),
            (403, _) => Err("The HomeStack key lacks Pangolin resource/target permissions. Verify Step 7.".to_owned()),
            (status, _) => Err(format!(
                "Pangolin GET {path} returned HTTP {status}; refresh and review again"
            )),
        }
    }

    fn rows(&self, field: &str) -> Result<Vec<Value>, String> {
        let field_key = if field == "private-resources" {
            "siteResources"
        } else {
            field
        };
        let mut rows = Vec::new();
        for page in 1..=1000 {
            let data = self.get(&format!(
                "/v1/org/{}/{field}?pageSize={PAGE}&page={page}",
                self.ctx.organization
            ))?;
            let items = data[field_key]
                .as_array()
                .filter(|items| items.len() <= PAGE)
                .ok_or("Unexpected Pangolin listing; response omitted")?;
            if data["pagination"]["page"].as_u64() != Some(page)
                || data["pagination"]["pageSize"].as_u64() != Some(PAGE as u64)
            {
                return Err("Unexpected Pangolin pagination; refresh Step 3 discovery".to_owned());
            }
            rows.extend(items.iter().cloned());
            if items.len() < PAGE {
                return Ok(rows);
            }
        }
        Err("Too many Pangolin objects to list".to_owned())
    }

    fn catalog(&self) -> Result<Catalog, String> {
        let mut resources = Vec::new();
        for row in self.rows("resources")? {
            if row["fullDomain"].as_str().is_some_and(|s| !s.is_empty()) {
                resources.push(Resource {
                    id: id(&row, "resourceId")?,
                    domain: text(&row, "fullDomain")?,
                    name: text(&row, "name")?,
                });
            }
        }
        let mut sites = Vec::new();
        for row in self.rows("sites")? {
            if row["type"] == "newt" {
                sites.push(Site {
                    id: id(&row, "siteId")?,
                    name: text(&row, "name")?,
                    nice: text(&row, "niceId")?,
                    online: row["online"].as_bool().ok_or("Missing Site online state")?,
                });
            }
        }
        let domains = self.domains()?;
        resources.sort_by(|a, b| a.domain.cmp(&b.domain));
        sites.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Catalog {
            resources,
            sites,
            domains,
        })
    }

    fn domains(&self) -> Result<Vec<Domain>, String> {
        let mut domains = Vec::new();
        let listed = self.get(&format!("/v1/org/{}/domains", self.ctx.organization))?;
        for row in listed["domains"]
            .as_array()
            .ok_or("Unexpected Pangolin domain listing")?
        {
            if row["verified"] == true {
                domains.push(Domain {
                    id: text(row, "domainId")?,
                    base: text(row, "baseDomain")?,
                });
            }
        }
        domains.sort_by(|a, b| a.base.cmp(&b.base));
        if domains
            .windows(2)
            .any(|pair| pair[0].base.eq_ignore_ascii_case(&pair[1].base))
        {
            return Err("Pangolin returned duplicate verified domains; inspect its configuration".into());
        }
        Ok(domains)
    }

    fn resource(&self, row: &Value) -> Result<Resource, String> {
        if row["mode"] != "http" || row["enabled"] != true {
            return Err("The selected resource is not an enabled HTTP resource".to_owned());
        }
        Ok(Resource {
            id: id(row, "resourceId")?,
            domain: text(row, "fullDomain")?,
            name: text(row, "name")?,
        })
    }

    fn validate(&self, resource: Option<&Resource>, site: &Site) -> Result<(), String> {
        if let Some(resource) = resource {
            let current = self.get(&format!("/v1/resource/{}", resource.id))?;
            if current["orgId"] != self.ctx.organization || self.resource(&current)? != *resource {
                return Err("Pangolin resource identity changed; review again".to_owned());
            }
        }
        let current_site = self.get(&format!("/v1/site/{}", site.id))?;
        if current_site["orgId"] != self.ctx.organization
            || id(&current_site, "siteId")? != site.id
            || current_site["niceId"] != site.nice
            || current_site["online"] != site.online
            || current_site["type"] != "newt"
        {
            return Err("Pangolin Site identity changed; review again".to_owned());
        }
        Ok(())
    }

    /// The bypass rules `paths` still need on `resource`; all of them for one
    /// not created yet. Only the resource's own (inline) policy is changed.
    fn bypass(&self, resource: Option<u64>, paths: &[String]) -> Result<Bypass, String> {
        if paths.is_empty() {
            return Ok(Bypass::default());
        }
        let Some(resource) = resource else {
            return Ok(Bypass {
                missing: paths.to_vec(),
                enable: true,
                priority: 1,
            });
        };
        let row = self.get(&format!("/v1/resource/{resource}"))?;
        if !row["resourcePolicyId"].is_null() {
            return Err("The resource uses a shared Pangolin policy; add its bypass rules there".to_owned());
        }
        let listed = self.get(&format!("/v1/resource/{resource}/rules?limit=1000"))?;
        let rules = listed["rules"].as_array().ok_or("Unexpected Pangolin rule listing")?;
        let present = |path: &String| {
            rules.iter().any(|rule| {
                rule["action"] == "ACCEPT"
                    && rule["match"] == "PATH"
                    && rule["value"] == path.as_str()
                    && rule["enabled"] == true
            })
        };
        Ok(Bypass {
            missing: paths.iter().filter(|path| !present(path)).cloned().collect(),
            enable: row["applyRules"] != true,
            priority: rules
                .iter()
                .filter_map(|rule| rule["priority"].as_i64())
                .max()
                .unwrap_or(0)
                + 1,
        })
    }

    /// One mutation, never retried: a lost response may still mean it was applied.
    fn mutate(&self, method: &str, path: &str, body: &Value) -> Result<(), String> {
        let (status, response) = self
            .client()
            .request(method, path, Some(&body.to_string()), Probe::Organization)
            .map_err(|_| format!("Pangolin {method} {path} outcome is unknown; refresh before retrying"))?;
        let accepted = serde_json::from_slice::<Value>(&response)
            .ok()
            .is_some_and(|v| v["success"] == true);
        if ![200, 201].contains(&status) || !accepted {
            return Err(match status {
                403 => format!("The HomeStack key may not {method} {path}; refresh Step 7"),
                _ => format!("Pangolin did not confirm {method} {path} (HTTP {status}); refresh before retrying"),
            });
        }
        Ok(())
    }

    /// Multiple targets require an explicit routing design; never replace them implicitly.
    fn target(&self, resource: u64) -> Result<Option<Target>, String> {
        let rows = self
            .client()
            .collection(
                &format!("/v1/resource/{resource}/targets"),
                "targets",
                Probe::Organization,
            )
            .map_err(|e| e.message().to_owned())?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] => {
                let target_id = row
                    .get("targetId")
                    .and_then(Value::as_u64)
                    .filter(|id| *id > 0)
                    .ok_or("Unexpected Pangolin targetId")?;
                let row = self.get(&format!("/v1/target/{target_id}"))?;
                if id(&row, "resourceId")? != resource || row["mode"] != "http" || !row["hcEnabled"].is_boolean() {
                    return Err("The existing target is not a supported HTTP backend".to_owned());
                }
                let fields = [
                    "siteId",
                    "ip",
                    "mode",
                    "method",
                    "port",
                    "enabled",
                    "path",
                    "pathMatchType",
                    "rewritePath",
                    "rewritePathType",
                    "priority",
                    "hcEnabled",
                    "hcPath",
                    "hcScheme",
                    "hcMode",
                    "hcHostname",
                    "hcPort",
                    "hcInterval",
                    "hcUnhealthyInterval",
                    "hcTimeout",
                    "hcHeaders",
                    "hcFollowRedirects",
                    "hcMethod",
                    "hcStatus",
                    "hcTlsServerName",
                    "hcHealthyThreshold",
                    "hcUnhealthyThreshold",
                ];
                let body: Map<String, Value> = fields
                    .into_iter()
                    .filter_map(|key| {
                        row.get(key)
                            .filter(|v| !v.is_null())
                            .map(|v| (key.to_owned(), v.clone()))
                    })
                    .collect();
                Ok(Some(Target {
                    id: target_id,
                    body: Value::Object(body),
                }))
            }
            _ => Err("This resource has multiple targets; select a resource with at most one backend".to_owned()),
        }
    }
}

fn id(row: &Value, key: &str) -> Result<u64, String> {
    row[key]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("Unexpected Pangolin {key}"))
}

fn text(row: &Value, key: &str) -> Result<String, String> {
    row[key]
        .as_str()
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| format!("Unexpected Pangolin {key}"))
}

/// Select the verified base domain explicitly whenever more than one is available.
fn full_domain(catalog: &Catalog, subdomain: &str, selected: &str) -> Result<(String, String, String), String> {
    let selected = selected.trim().trim_end_matches('.');
    let domain = if selected.is_empty() {
        match catalog.domains.as_slice() {
            [only] => only,
            [] => return Err("Pangolin has no verified domain; add one in Pangolin first".into()),
            many => {
                return Err(format!(
                    "Choose Pangolin domain: {}",
                    many.iter().map(|d| d.base.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    } else {
        catalog
            .domains
            .iter()
            .find(|d| d.base.eq_ignore_ascii_case(selected))
            .ok_or_else(|| format!("Pangolin domain {selected} is not verified for this organization"))?
    };
    Ok((
        domain.id.clone(),
        domain.base.clone(),
        format!("{subdomain}.{}", domain.base),
    ))
}

pub(super) fn review(
    source: &Source,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    instances: &[Instance],
    created: &[NewInstance],
    publication: Publication,
) -> Result<Plan, String> {
    let (container, address) = match (instances, created) {
        ([instance], []) => {
            if !instance.running || instance.held || instance.address.parse::<std::net::IpAddr>().is_err() {
                return Err("Select a running instance with a known IP address".to_owned());
            }
            (
                Container {
                    ctid: instance.ctid,
                    node: instance.node.clone(),
                    existing: Some(instance.clone()),
                },
                instance.address.clone(),
            )
        }
        ([], [new]) => (
            Container {
                ctid: new.ctid,
                node: new.node.clone(),
                existing: None,
            },
            new.address.clone(),
        ),
        _ => {
            return Err(
                "Publishing needs exactly one instance: select one existing instance or create one new".to_owned(),
            );
        }
    };
    plan(source, pangolin, stop, Some(container), address, None, publication)
}

/// Route a service without resources to `address` through the Site whose niceId is `site`.
pub(super) fn review_existing(
    source: &Source,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    address: String,
    site: &str,
    publication: Publication,
) -> Result<Plan, String> {
    plan(source, pangolin, stop, None, address, Some(site), publication)
}

fn plan(
    source: &Source,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    container: Option<Container>,
    address: String,
    site: Option<&str>,
    publication: Publication,
) -> Result<Plan, String> {
    let api = Api::open(pangolin, stop)?;
    plan_with_api(source, &api, container, address, site, publication)
}

fn plan_with_api(
    source: &Source,
    api: &Api<'_>,
    container: Option<Container>,
    address: String,
    site: Option<&str>,
    publication: Publication,
) -> Result<Plan, String> {
    let exposure = source
        .manifest
        .pangolin
        .as_ref()
        .ok_or("The service manifest declares no Pangolin exposure")?;
    let port = source
        .manifest
        .ports
        .iter()
        .find(|p| p.name == exposure.port)
        .ok_or("The manifest's Pangolin exposure refers to an undeclared port")?
        .port;
    let catalog = api.catalog()?;
    check_subdomain(&publication.subdomain)?;
    let (domain_id, base_domain, domain) = full_domain(&catalog, &publication.subdomain, &publication.domain)?;
    let publication = Publication {
        domain: base_domain.clone(),
        ..publication
    };
    let public_receipt = api.ctx.key.with_file_name(format!(
        "{}.created.json",
        private::identifier("public", &api.ctx.identity, source, &domain)
    ));
    let resource = private::public_match(&catalog.resources, &domain)?;
    let previous = resource.as_ref().map(|r| api.target(r.id)).transpose()?.flatten();
    let keep_public = publication.paired && resource.is_some() && previous.is_some();
    let public_snapshot = if publication.paired {
        resource
            .as_ref()
            .map(|r| private::public_snapshot(&api, r, &domain_id, &publication.subdomain))
            .transpose()?
    } else {
        None
    };
    let site = if keep_public {
        let target = previous.as_ref().ok_or("The Public Resource has no target")?;
        private::validate_target(&target.body, &address, port)?;
        catalog
            .sites
            .iter()
            .find(|s| Some(s.id) == target.body["siteId"].as_u64())
            .cloned()
            .ok_or("The existing Public Resource Site is unavailable; no routing was changed")?
    } else {
        match site {
            Some(name) => catalog
                .sites
                .iter()
                .find(|s| s.nice == name && s.online)
                .cloned()
                .ok_or_else(|| format!("Pangolin Site {name} is not online"))?,
            None => catalog
                .sites
                .iter()
                .find(|s| s.online)
                .cloned()
                .ok_or("Pangolin has no online Sites; start a Site connector first")?,
        }
    };
    api.validate(resource.as_ref(), &site)?;
    if publication.paired && resource.is_some() && previous.is_none() {
        let snapshot = public_snapshot.as_ref().ok_or("Missing Public Resource identity")?;
        if !private::owns_public(&public_receipt, &snapshot["resource"])? {
            return Err("The existing Public Resource has no target and is not identified as this deployment's creation; repair it explicitly".into());
        }
    }
    let bypass = api.bypass(resource.as_ref().map(|r| r.id), &exposure.bypass_paths)?;
    if keep_public && (!bypass.missing.is_empty() || bypass.enable) {
        return Err("The existing Public Resource lacks the manifest's enabled login-bypass rules; review and repair its public policy explicitly. Paired exposure never changes it".into());
    }
    let create = resource.is_none().then(|| {
        json!({
            "name": exposure.name,
            "subdomain": publication.subdomain,
            "domainId": domain_id,
            "mode": "http",
        })
    });
    let mut body = previous
        .as_ref()
        .map(|t| t.body.clone())
        .unwrap_or_else(|| json!({"enabled": true}));
    body["siteId"] = json!(site.id);
    body["ip"] = json!(address);
    body["mode"] = json!("http");
    body["method"] = json!("http");
    body["port"] = json!(port);
    let Some(Healthcheck::Http {
        port: health_port,
        path,
        expected_status,
        ..
    }) = &source.manifest.healthcheck
    else {
        return Err("Pangolin HTTP monitoring requires an HTTP health check in the service manifest".to_owned());
    };
    let health_port = source
        .manifest
        .ports
        .iter()
        .find(|p| &p.name == health_port)
        .ok_or("The manifest health check refers to an undeclared port")?
        .port;
    body["hcEnabled"] = json!(true);
    body["hcMode"] = json!("http");
    body["hcHostname"] = json!(address);
    body["hcPort"] = json!(health_port);
    body["hcScheme"] = json!("http");
    body["hcPath"] = json!(path);
    body["hcMethod"] = json!("GET");
    body["hcStatus"] = json!(expected_status);
    body["hcInterval"] = json!(30);
    body["hcUnhealthyInterval"] = json!(30);
    body["hcTimeout"] = json!(5);
    body["hcFollowRedirects"] = json!(false);
    body["hcHealthyThreshold"] = json!(1);
    body["hcUnhealthyThreshold"] = json!(3);
    if keep_public {
        body = previous.as_ref().ok_or("Missing reviewed Public target")?.body.clone();
    }
    let private = if publication.paired {
        Some(private::review(
            &api,
            &catalog,
            source,
            &publication,
            &domain,
            &domain_id,
            &body,
            &site,
        )?)
    } else {
        None
    };
    Ok(Plan {
        keep_public,
        public_snapshot,
        public_receipt,
        private,
        container,
        address,
        repository: source.repository.clone(),
        service: source.manifest.id.clone(),
        domain,
        domain_id,
        base_domain,
        publication,
        resource,
        create,
        site,
        previous,
        body,
        bypass_paths: exposure.bypass_paths.clone(),
        bypass,
    })
}

impl Plan {
    /// The full domain the service is published under.
    pub(super) fn domain(&self) -> &str {
        &self.domain
    }

    pub(super) fn keeps_public(&self) -> bool {
        self.keep_public
    }

    pub(super) fn unchanged(&self) -> bool {
        self.keep_public && self.private.as_ref().is_some_and(|p| p.existing.is_some())
    }

    fn target_unchanged(&self) -> bool {
        self.previous.as_ref().is_some_and(|target| target.body == self.body)
    }

    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Public URL: https://{}", self.domain),
            format!(
                "https://{} → Site {} (ID {}) → http://{}:{}{}",
                self.domain,
                self.site.name,
                self.site.id,
                self.body["ip"].as_str().unwrap_or(&self.address),
                self.body["port"],
                self.container
                    .as_ref()
                    .map_or_else(String::new, |c| format!(" · CT {} on {}", c.ctid, c.node))
            ),
            match (&self.resource, &self.previous) {
                (None, _) => {
                    "Public Resource: CREATE with Pangolin's default access policy (Pangolin login)".to_owned()
                }
                (Some(_), Some(target)) if self.target_unchanged() => {
                    format!(
                        "Public Resource: KEEP; target {} unchanged; SSO, access policies and health settings preserved",
                        target.id
                    )
                }
                (Some(_), Some(target)) => format!("Update existing backend target {}", target.id),
                (Some(_), None) => "Create the missing backend target".to_owned(),
            },
            if self.keep_public {
                "No public target, policy, health-check or certificate writes".into()
            } else {
                format!(
                    "Monitoring enabled: GET http://{}:{}{} expects HTTP {}",
                    self.address,
                    self.body["hcPort"],
                    self.body["hcPath"].as_str().unwrap_or_default(),
                    self.body["hcStatus"]
                )
            },
        ];
        if let Some(private) = &self.private {
            lines.extend(private.lines(&self.domain));
        }
        if self.unchanged() {
            lines.push("No Pangolin changes: the Public + Private pair is already correct".into());
        }
        lines.extend(
            self.bypass
                .missing
                .iter()
                .map(|path| format!("Add rule: {path} bypasses Pangolin login")),
        );
        if self.bypass.enable {
            lines.push("Turn on rule evaluation for the resource".to_owned());
        }
        lines.push(if self.container.is_some() {
            "Other access policies of an existing resource, the container, its disks and persistent volume are preserved."
                .to_owned()
        } else {
            "Other access policies of an existing resource are preserved; the service itself is not touched.".to_owned()
        });
        lines
    }
}

pub(super) async fn apply(
    pve: &impl Pve,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    plan: &Plan,
) -> Result<Vec<String>, String> {
    if let Some(container) = &plan.container {
        let now = instance::read(pve, container.ctid).await?;
        if now.repository != plan.repository
            || now.service != plan.service
            || now.address != plan.address
            || container
                .existing
                .as_ref()
                .is_some_and(|known| now.mac != known.mac || now.home_volume != known.home_volume)
            || !now.running
            || now.held
        {
            return Err("The selected container changed since review; review again".to_owned());
        }
    }
    tokio::task::block_in_place(|| route(pangolin, stop, plan))
}

/// Create or reuse the reviewed resource, its target and its bypass rules.
fn route(pangolin: &Pangolin, stop: &AtomicBool, plan: &Plan) -> Result<Vec<String>, String> {
    let api = Api::open(pangolin, stop)?;
    route_with_api(&api, plan)
}

fn route_with_api(api: &Api<'_>, plan: &Plan) -> Result<Vec<String>, String> {
    let catalog = api.catalog()?;
    if !catalog
        .domains
        .iter()
        .any(|d| d.id == plan.domain_id && d.base == plan.base_domain)
    {
        return Err("The selected Pangolin domain changed since review; review again".into());
    }
    let listed = catalog.resources.iter().find(|r| r.domain == plan.domain);
    if listed != plan.resource.as_ref()
        || !catalog.sites.contains(&plan.site)
        || plan.resource.as_ref().map(|r| api.target(r.id)).transpose()?.flatten() != plan.previous
    {
        return Err("Pangolin routing changed since review; review again".to_owned());
    }
    api.validate(plan.resource.as_ref(), &plan.site)?;
    if let Some(private) = &plan.private {
        private.validate(&api, &catalog, plan)?;
    }
    if let (Some(snapshot), Some(resource)) = (&plan.public_snapshot, &plan.resource) {
        if private::public_snapshot(&api, resource, &plan.domain_id, &plan.publication.subdomain)? != *snapshot {
            return Err("The Public Resource or its access policies changed since review; review again".into());
        }
    }
    if plan.keep_public {
        let mut lines = vec!["Public Resource, target, SSO, policies and health settings preserved".into()];
        if let Some(private) = &plan.private {
            lines.extend(private.apply(&api, plan)?);
        }
        return Ok(lines);
    }
    let mut lines = Vec::new();
    let resource = match (&plan.resource, &plan.create) {
        (Some(resource), _) => resource.id,
        (None, Some(create)) => {
            let path = format!("/v1/org/{}/resource", api.ctx.organization);
            // Never retry a mutation: a lost response may still mean it was applied.
            let (status, response) = api
                .client()
                .request("PUT", &path, Some(&create.to_string()), Probe::Organization)
                .map_err(|_| "Pangolin resource creation outcome is unknown; review again".to_owned())?;
            let created = serde_json::from_slice::<Value>(&response).unwrap_or_default();
            if ![200, 201].contains(&status)
                || created["success"] != true
                || created["data"]["fullDomain"] != plan.domain.as_str()
            {
                return Err(match status {
                    403 => "The HomeStack key may not create resources (createResource); refresh Step 7".to_owned(),
                    _ => format!(
                        "Pangolin did not create resource {} (HTTP {status}); review again",
                        plan.domain
                    ),
                });
            }
            let rid = id(&created["data"], "resourceId")?;
            if plan.private.is_some() {
                let row = api.get(&format!("/v1/resource/{rid}"))?;
                if row["orgId"] != api.ctx.organization
                    || row["domainId"] != plan.domain_id
                    || row["fullDomain"] != plan.domain
                    || row["ssl"] != true
                    || row["sso"] != true
                    || row["mode"] != "http"
                    || row["enabled"] != true
                {
                    return Err("New Public Resource defaults do not match enabled HTTPS with SSO; inspect the resource before continuing".into());
                }
                private::remember_public(&plan.public_receipt, &row)?;
            }
            lines.push(format!("Resource {} created", plan.domain));
            rid
        }
        (None, None) => return Err("The reviewed plan has no resource".to_owned()),
    };
    let bypass = api.bypass(Some(resource), &plan.bypass_paths)?;
    if plan.resource.is_some() && bypass != plan.bypass {
        return Err("Pangolin rules changed since review; review again".to_owned());
    }
    if !plan.target_unchanged() {
        save_target(&api, plan, resource)?;
    }
    lines.push(format!(
        "https://{} routed through {} to {}:{}{}",
        plan.domain,
        plan.site.name,
        plan.address,
        plan.body["port"],
        if plan.container.is_some() {
            "; container and persistent volume preserved"
        } else {
            ""
        }
    ));
    for (path, priority) in bypass.missing.iter().zip(bypass.priority..) {
        let rule = json!({"action": "ACCEPT", "match": "PATH", "value": path, "priority": priority, "enabled": true});
        api.mutate("PUT", &format!("/v1/resource/{resource}/rule"), &rule)?;
        lines.push(format!("{path} bypasses Pangolin login"));
    }
    if bypass.enable {
        api.mutate(
            "POST",
            &format!("/v1/resource/{resource}"),
            &json!({"applyRules": true}),
        )?;
        lines.push("Rule evaluation turned on".to_owned());
    }
    let saved = api.bypass(Some(resource), &plan.bypass_paths)?;
    if !saved.missing.is_empty() || saved.enable {
        return Err("Saved Pangolin rules do not match the review; inspect the resource rules".to_owned());
    }
    if let Some(private) = &plan.private {
        lines.extend(private.apply(&api, plan)?);
    }
    Ok(lines)
}

/// Create or update the backend target, then verify it reads back as reviewed.
fn save_target(api: &Api, plan: &Plan, resource: u64) -> Result<(), String> {
    if api.target(resource)? != plan.previous {
        return Err("The Public target changed before saving; review again".into());
    }
    let (method, path) = match &plan.previous {
        Some(target) => ("POST", format!("/v1/target/{}", target.id)),
        None => ("PUT", format!("/v1/resource/{resource}/target")),
    };
    let body = plan.body.to_string();
    let (status, response) = api
        .client()
        .request(method, &path, Some(&body), Probe::Organization)
        .map_err(|_| {
            "Pangolin save outcome is unknown. Refresh routing before retrying; the container is unchanged".to_owned()
        })?;
    let accepted = serde_json::from_slice::<Value>(&response)
        .ok()
        .is_some_and(|v| v["success"] == true && v["error"] == false);
    if ![200, 201].contains(&status) || !accepted {
        return Err(format!(
            "Pangolin did not confirm saving the target (HTTP {status}). Refresh before retrying; the container is unchanged"
        ));
    }
    let saved = api
        .target(resource)?
        .ok_or("Pangolin did not return the saved target; refresh before retrying")?;
    if plan.previous.as_ref().is_some_and(|old| old.id != saved.id)
        || plan
            .body
            .as_object()
            .is_none_or(|expected| expected.iter().any(|(key, value)| saved.body.get(key) != Some(value)))
    {
        return Err("Saved Pangolin target does not match the review; inspect routing before retrying".to_owned());
    }
    Ok(())
}
