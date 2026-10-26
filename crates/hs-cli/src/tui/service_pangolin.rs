//! Attach an existing service to an existing Pangolin HTTP resource. This path
//! reads Proxmox and changes only the resource's backend target through Step 7.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use hs_core::pve::Pve;
use serde_json::{Map, Value, json};

use super::install::Setting;
use super::pangolin_key::{Client, Credential, Probe};
use super::pangolin_sites::{Context, managed::Pangolin};
use crate::install_flow::Answer;
use crate::services::{
    deploy::Source,
    instance::{self, Instance},
    manifest::Healthcheck,
};

/// The title of the first Pangolin setting; the others follow it.
pub(super) const ACTION: &str = "Pangolin";
const RESOURCE: usize = 1;
const SITE: usize = 2;
const PORT: usize = 3;
const SCHEME: usize = 4;

/// Where the Pangolin settings start among `settings`.
pub(super) fn start(settings: &[Setting]) -> Option<usize> {
    settings.iter().position(|setting| setting.title == ACTION)
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
}

pub(super) struct Catalog {
    resources: Vec<Resource>,
    sites: Vec<Site>,
}

pub(super) struct Plan {
    instance: Instance,
    resource: Resource,
    site: Site,
    previous: Option<Target>,
    body: Value,
}

#[derive(PartialEq)]
struct Target {
    id: u64,
    body: Value,
}

pub(super) fn requested(settings: &[Setting], answers: &[Answer]) -> bool {
    start(settings)
        .is_some_and(|at| matches!(answers.get(at), Some(Answer::Picked(indices)) if indices.as_slice() == [1]))
}

fn setting(title: &str, note: &str, choices: Vec<(String, String)>, text: Option<String>) -> Setting {
    Setting {
        title: title.to_owned(),
        note: note.to_owned(),
        choices,
        text,
        list: false,
        check: None,
        application_input: None,
        auto_text: false,
        hint: None,
        multiple: false,
        picked: Vec::new(),
    }
}

pub(super) fn settings(source: &Source, catalog: Option<&Catalog>) -> Vec<Setting> {
    if source.is_site() || source.manifest.pangolin.is_none() {
        return Vec::new();
    }
    let mut action = setting(
        ACTION,
        "Choose attach, then Enter to load current resources and online Sites. Select one existing instance, \
         a resource and a Site, set the application's port and protocol, then review. This changes only \
         Pangolin routing; the container and /home/app are preserved. Unchanged makes no Pangolin request.",
        vec![
            ("unchanged".to_owned(), "Keep current routing".to_owned()),
            ("attach".to_owned(), "Attach an existing instance".to_owned()),
        ],
        None,
    );
    action.picked = vec![0];
    let mut settings = vec![action];
    if let Some(catalog) = catalog {
        settings.extend([
            setting("Pangolin resource", "The existing public HTTP resource. Its domain and access policies stay as configured in Pangolin.",
                catalog.resources.iter().map(|r| (r.id.to_string(), format!("{} · {}", r.name, r.domain))).collect(), None),
            setting("Pangolin Site", "An online Site that can reach the selected container's IP address. F5 refreshes the list after Site recreation.",
                catalog.sites.iter().map(|s| (s.id.to_string(), if s.name == s.nice { s.name.clone() } else { format!("{} · {}", s.name, s.nice) })).collect(), None),
            setting("Pangolin port", "The port the application listens on inside the existing container (1–65535). Its IP is read from Proxmox.", Vec::new(),
                Some(source.manifest.ports.first().map(|p| p.port.to_string()).unwrap_or_default())),
            setting("Pangolin protocol", "HTTP or HTTPS between the Site and the application; this does not change the public domain's TLS configuration.",
                vec![("http".to_owned(), String::new()), ("https".to_owned(), String::new())], None),
        ]);
        if let Some(protocol) = settings.last_mut() {
            protocol.picked = vec![0];
        }
    }
    settings
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
        let mut rows = Vec::new();
        for page in 1..=1000 {
            let data = self.get(&format!(
                "/v1/org/{}/{field}?pageSize={PAGE}&page={page}",
                self.ctx.organization
            ))?;
            let items = data[field]
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
            if row["mode"] == "http" && row["enabled"] == true {
                resources.push(self.resource(&row)?);
            }
        }
        let mut sites = Vec::new();
        for row in self.rows("sites")? {
            if row["online"] == true && row["type"] == "newt" {
                sites.push(Site {
                    id: id(&row, "siteId")?,
                    name: text(&row, "name")?,
                    nice: text(&row, "niceId")?,
                });
            }
        }
        resources.sort_by(|a, b| a.domain.cmp(&b.domain));
        sites.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Catalog { resources, sites })
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

    fn validate(&self, resource: &Resource, site: &Site) -> Result<(), String> {
        let current_resource = self.get(&format!("/v1/resource/{}", resource.id))?;
        let current_site = self.get(&format!("/v1/site/{}", site.id))?;
        if current_resource["orgId"] != self.ctx.organization
            || current_site["orgId"] != self.ctx.organization
            || self.resource(&current_resource)? != *resource
            || id(&current_site, "siteId")? != site.id
            || current_site["niceId"] != site.nice
            || current_site["online"] != true
            || current_site["type"] != "newt"
        {
            return Err("Pangolin resource or Site identity changed; refresh and review again".to_owned());
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

pub(super) fn load(pangolin: &Pangolin, stop: &AtomicBool) -> Result<Catalog, String> {
    let catalog = Api::open(pangolin, stop)?.catalog()?;
    if catalog.resources.is_empty() {
        return Err(
            "Pangolin has no enabled HTTP resources. Create a resource in Pangolin before attaching an instance"
                .to_owned(),
        );
    }
    if catalog.sites.is_empty() {
        return Err("Pangolin has no online Sites. Start a Site connector before attaching an instance".to_owned());
    }
    Ok(catalog)
}

pub(super) fn review(
    source: &Source,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    catalog: Option<&Catalog>,
    settings: &[Setting],
    answers: &[Answer],
    instances: &[Instance],
) -> Result<Plan, String> {
    let catalog = catalog.ok_or("Choose attach and press Enter to load Pangolin resources and Sites")?;
    let [instance] = instances else {
        return Err("Pangolin attachment requires exactly one selected instance".to_owned());
    };
    if !instance.running || instance.held || instance.address.parse::<std::net::IpAddr>().is_err() {
        return Err("Select a running instance with a known IP address".to_owned());
    }
    let base = start(settings).ok_or("The Pangolin settings are missing")?;
    let selected = |at: usize| -> Result<&str, String> {
        match answers.get(at) {
            Some(Answer::Picked(indices)) if indices.len() == 1 => settings
                .get(at)
                .and_then(|s| s.choices.get(indices[0]))
                .map(|(key, _)| key.as_str())
                .ok_or("Invalid Pangolin selection".to_owned()),
            _ => Err("Select a Pangolin resource, Site and protocol".to_owned()),
        }
    };
    let resource = catalog
        .resources
        .iter()
        .find(|r| r.id.to_string() == selected(base + RESOURCE).unwrap_or_default())
        .ok_or("Select a Pangolin resource")?
        .clone();
    let site = catalog
        .sites
        .iter()
        .find(|s| s.id.to_string() == selected(base + SITE).unwrap_or_default())
        .ok_or("Select an online Pangolin Site")?
        .clone();
    let port = match answers.get(base + PORT) {
        Some(Answer::Text(value)) => value.trim().parse::<u16>().ok().filter(|p| *p > 0),
        _ => None,
    }
    .ok_or("Pangolin port must be a number between 1 and 65535")?;
    let scheme = selected(base + SCHEME)?;
    if !["http", "https"].contains(&scheme) {
        return Err("Choose HTTP or HTTPS".to_owned());
    }
    let api = Api::open(pangolin, stop)?;
    let current = api.catalog()?;
    if !current.resources.contains(&resource) || !current.sites.contains(&site) {
        return Err("Pangolin resources or Sites changed; press F5 and select again".to_owned());
    }
    api.validate(&resource, &site)?;
    let previous = api.target(resource.id)?;
    let mut body = previous
        .as_ref()
        .map(|t| t.body.clone())
        .unwrap_or_else(|| json!({"enabled": true}));
    body["siteId"] = json!(site.id);
    body["ip"] = json!(instance.address);
    body["mode"] = json!("http");
    body["method"] = json!(scheme);
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
    body["hcHostname"] = json!(instance.address);
    body["hcPort"] = json!(health_port);
    body["hcScheme"] = json!(if health_port == port { scheme } else { "http" });
    body["hcPath"] = json!(path);
    body["hcMethod"] = json!("GET");
    body["hcStatus"] = json!(expected_status);
    body["hcInterval"] = json!(30);
    body["hcUnhealthyInterval"] = json!(30);
    body["hcTimeout"] = json!(5);
    body["hcFollowRedirects"] = json!(false);
    body["hcHealthyThreshold"] = json!(1);
    body["hcUnhealthyThreshold"] = json!(3);
    Ok(Plan {
        instance: instance.clone(),
        resource,
        site,
        previous,
        body,
    })
}

impl Plan {
    pub(super) fn lines(&self) -> Vec<String> {
        vec![
            format!(
                "{} → Site {} (ID {}) → {}://{}:{} · CT {} on {}",
                self.resource.domain,
                self.site.name,
                self.site.id,
                self.body["method"].as_str().unwrap_or_default(),
                self.instance.address,
                self.body["port"],
                self.instance.ctid,
                self.instance.node
            ),
            if let Some(target) = &self.previous {
                format!("Update existing backend target {}", target.id)
            } else {
                "Create the missing backend target".to_owned()
            },
            format!(
                "Monitoring enabled: GET {}://{}:{}{} expects HTTP {}",
                self.body["hcScheme"].as_str().unwrap_or_default(),
                self.instance.address,
                self.body["hcPort"],
                self.body["hcPath"].as_str().unwrap_or_default(),
                self.body["hcStatus"]
            ),
            "Only Pangolin routing changes. Resource access policies, container, disks and /home/app are preserved."
                .to_owned(),
        ]
    }
}

pub(super) async fn apply(
    pve: &impl Pve,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    plan: &Plan,
) -> Result<Vec<String>, String> {
    let now = instance::read(pve, plan.instance.ctid).await?;
    if now.repository != plan.instance.repository
        || now.service != plan.instance.service
        || now.mac != plan.instance.mac
        || now.home_volume != plan.instance.home_volume
        || now.address != plan.instance.address
        || !now.running
        || now.held
    {
        return Err("The selected container changed since review; review again".to_owned());
    }
    tokio::task::block_in_place(|| {
        let api = Api::open(pangolin, stop)?;
        let catalog = api.catalog()?;
        if !catalog.resources.contains(&plan.resource)
            || !catalog.sites.contains(&plan.site)
            || api.target(plan.resource.id)? != plan.previous
        {
            return Err("Pangolin routing changed since review; refresh and review again".to_owned());
        }
        api.validate(&plan.resource, &plan.site)?;
        let (method, path) = match &plan.previous {
            Some(target) => ("POST", format!("/v1/target/{}", target.id)),
            None => ("PUT", format!("/v1/resource/{}/target", plan.resource.id)),
        };
        let body = plan.body.to_string();
        // Never retry a mutation: a lost response may still mean it was applied.
        let (status, response) = api
            .client()
            .request(method, &path, Some(&body), Probe::Organization)
            .map_err(|_| {
                "Pangolin save outcome is unknown. Refresh routing before retrying; the container is unchanged"
                    .to_owned()
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
            .target(plan.resource.id)?
            .ok_or("Pangolin did not return the saved target; refresh before retrying")?;
        if plan.previous.as_ref().is_some_and(|old| old.id != saved.id)
            || plan
                .body
                .as_object()
                .is_none_or(|expected| expected.iter().any(|(key, value)| saved.body.get(key) != Some(value)))
        {
            return Err("Saved Pangolin target does not match the review; inspect routing before retrying".to_owned());
        }
        Ok(vec![format!(
            "{} attached through {} to {}:{}; existing container and /home/app preserved",
            plan.resource.domain, plan.site.name, plan.instance.address, plan.body["port"]
        )])
    })
}
