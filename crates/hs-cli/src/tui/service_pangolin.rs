//! Publish a service through Pangolin: find or create the HTTP resource its
//! manifest names and point the resource's backend target at one instance.
//! This reads Proxmox and changes only Pangolin routing through Step 7.

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

struct Domain {
    id: String,
    base: String,
}

struct Catalog {
    resources: Vec<Resource>,
    sites: Vec<Site>,
    domains: Vec<Domain>,
}

pub(super) struct Plan {
    ctid: u32,
    node: String,
    address: String,
    repository: String,
    service: String,
    /// The existing container's reviewed identity; `None` for an instance
    /// this deployment creates before attaching.
    existing: Option<Instance>,
    /// The resource's full domain.
    domain: String,
    /// The existing resource; `None` when Apply creates it from `create`.
    resource: Option<Resource>,
    create: Option<Value>,
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
    settings
        .iter()
        .position(|setting| setting.title == ACTION)
        .is_some_and(|at| matches!(answers.get(at), Some(Answer::Picked(indices)) if indices.as_slice() == [1]))
}

pub(super) fn settings(source: &Source) -> Vec<Setting> {
    let Some(exposure) = source.manifest.pangolin.as_ref().filter(|_| !source.is_site()) else {
        return Vec::new();
    };
    vec![Setting {
        title: ACTION.to_owned(),
        note: format!(
            "Publish reuses or creates the Pangolin HTTP resource {sub} on the organization's domain (with \
             several, the one most resources use), routes it through an online Site to one instance (the \
             selected one, or the one this deployment creates) on its {port} port and monitors its health \
             check. Unchanged makes no Pangolin request.",
            sub = exposure.subdomain,
            port = exposure.port,
        ),
        choices: vec![
            ("unchanged".to_owned(), "Keep current routing".to_owned()),
            ("attach".to_owned(), format!("Publish {}", exposure.subdomain)),
        ],
        text: None,
        list: false,
        check: None,
        application_input: None,
        auto_text: false,
        hint: None,
        multiple: false,
        picked: vec![0],
    }]
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
        resources.sort_by(|a, b| a.domain.cmp(&b.domain));
        sites.sort_by(|a, b| a.name.cmp(&b.name));
        domains.sort_by(|a, b| a.base.cmp(&b.base));
        Ok(Catalog {
            resources,
            sites,
            domains,
        })
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
            || current_site["online"] != true
            || current_site["type"] != "newt"
        {
            return Err("Pangolin Site identity changed; review again".to_owned());
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

/// The full domain of `subdomain` on the organization's domain: the one
/// with a resource of that name, else the one most resources use.
fn full_domain(catalog: &Catalog, subdomain: &str) -> Result<(String, String), String> {
    let used = |base: &str| {
        catalog
            .resources
            .iter()
            .filter(|r| r.domain.ends_with(&format!(".{base}")) || r.domain == base)
            .count()
    };
    let domain = catalog
        .domains
        .iter()
        .find(|d| {
            catalog
                .resources
                .iter()
                .any(|r| r.domain == format!("{subdomain}.{}", d.base))
        })
        .or_else(|| catalog.domains.iter().rev().max_by_key(|d| used(&d.base)))
        .ok_or("Pangolin has no verified domain; add one in Pangolin first")?;
    Ok((domain.id.clone(), format!("{subdomain}.{}", domain.base)))
}

pub(super) fn review(
    source: &Source,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    instances: &[Instance],
    created: &[NewInstance],
) -> Result<Plan, String> {
    let exposure = source
        .manifest
        .pangolin
        .as_ref()
        .ok_or("The service manifest declares no Pangolin exposure")?;
    let (ctid, node, address, existing) = match (instances, created) {
        ([instance], []) => {
            if !instance.running || instance.held || instance.address.parse::<std::net::IpAddr>().is_err() {
                return Err("Select a running instance with a known IP address".to_owned());
            }
            (
                instance.ctid,
                instance.node.clone(),
                instance.address.clone(),
                Some(instance.clone()),
            )
        }
        ([], [new]) => (new.ctid, new.node.clone(), new.address.clone(), None),
        _ => {
            return Err(
                "Publishing needs exactly one instance: select one existing instance or create one new".to_owned(),
            );
        }
    };
    let port = source
        .manifest
        .ports
        .iter()
        .find(|p| p.name == exposure.port)
        .ok_or("The manifest's Pangolin exposure refers to an undeclared port")?
        .port;
    let api = Api::open(pangolin, stop)?;
    let catalog = api.catalog()?;
    let (domain_id, domain) = full_domain(&catalog, &exposure.subdomain)?;
    let resource = catalog.resources.iter().find(|r| r.domain == domain).cloned();
    let site = catalog
        .sites
        .first()
        .cloned()
        .ok_or("Pangolin has no online Sites; start a Site connector first")?;
    api.validate(resource.as_ref(), &site)?;
    let previous = match &resource {
        Some(resource) => api.target(resource.id)?,
        None => None,
    };
    let create = resource.is_none().then(|| {
        json!({
            "name": exposure.name,
            "subdomain": exposure.subdomain,
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
    Ok(Plan {
        ctid,
        node,
        address,
        repository: source.repository.clone(),
        service: source.manifest.id.clone(),
        existing,
        domain,
        resource,
        create,
        site,
        previous,
        body,
    })
}

impl Plan {
    /// The full domain the service is published under.
    pub(super) fn domain(&self) -> &str {
        &self.domain
    }

    pub(super) fn lines(&self) -> Vec<String> {
        vec![
            format!(
                "https://{} → Site {} (ID {}) → http://{}:{} · CT {} on {}",
                self.domain, self.site.name, self.site.id, self.address, self.body["port"], self.ctid, self.node
            ),
            match (&self.resource, &self.previous) {
                (None, _) => "Create the resource with Pangolin's default access policy (Pangolin login)".to_owned(),
                (Some(_), Some(target)) => format!("Update existing backend target {}", target.id),
                (Some(_), None) => "Create the missing backend target".to_owned(),
            },
            format!(
                "Monitoring enabled: GET http://{}:{}{} expects HTTP {}",
                self.address,
                self.body["hcPort"],
                self.body["hcPath"].as_str().unwrap_or_default(),
                self.body["hcStatus"]
            ),
            "Access policies of an existing resource, the container, disks and /home/app are preserved.".to_owned(),
        ]
    }
}

pub(super) async fn apply(
    pve: &impl Pve,
    pangolin: &Pangolin,
    stop: &AtomicBool,
    plan: &Plan,
) -> Result<Vec<String>, String> {
    let now = instance::read(pve, plan.ctid).await?;
    if now.repository != plan.repository
        || now.service != plan.service
        || now.address != plan.address
        || plan
            .existing
            .as_ref()
            .is_some_and(|known| now.mac != known.mac || now.home_volume != known.home_volume)
        || !now.running
        || now.held
    {
        return Err("The selected container changed since review; review again".to_owned());
    }
    tokio::task::block_in_place(|| {
        let api = Api::open(pangolin, stop)?;
        let catalog = api.catalog()?;
        let listed = catalog.resources.iter().find(|r| r.domain == plan.domain);
        if listed != plan.resource.as_ref()
            || !catalog.sites.contains(&plan.site)
            || plan.resource.as_ref().map(|r| api.target(r.id)).transpose()?.flatten() != plan.previous
        {
            return Err("Pangolin routing changed since review; review again".to_owned());
        }
        api.validate(plan.resource.as_ref(), &plan.site)?;
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
                lines.push(format!("Resource {} created", plan.domain));
                id(&created["data"], "resourceId")?
            }
            (None, None) => return Err("The reviewed plan has no resource".to_owned()),
        };
        let (method, path) = match &plan.previous {
            Some(target) => ("POST", format!("/v1/target/{}", target.id)),
            None => ("PUT", format!("/v1/resource/{resource}/target")),
        };
        let body = plan.body.to_string();
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
        lines.push(format!(
            "https://{} routed through {} to {}:{}; container and /home/app preserved",
            plan.domain, plan.site.name, plan.address, plan.body["port"]
        ));
        Ok(lines)
    })
}
