//! Publish a service through Pangolin: find or create the HTTP resource its
//! manifest names, point the resource's backend target at one instance and
//! add the manifest's login-bypass path rules.
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
pub(super) const DOMAIN: &str = "Pangolin domain";
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
    domain_id: String,
    base_domain: String,
    /// The existing resource; `None` when Apply creates it from `create`.
    resource: Option<Resource>,
    create: Option<Value>,
    site: Site,
    previous: Option<Target>,
    body: Value,
    bypass_paths: Vec<String>,
    bypass: Bypass,
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
            "Publish reuses or creates the Pangolin HTTP resource {sub} on the selected Pangolin domain, \
             routes it through an online Site to one instance (the \
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
    };
    let mut domain = action.clone();
    domain.title = DOMAIN.to_owned();
    domain.note = "Press Enter to connect to Pangolin and choose a verified domain.".to_owned();
    domain.choices.clear();
    domain.picked.clear();
    vec![action, domain]
}

/// Load verified domains with the same organization-scoped key used for routing.
pub(super) fn domains(pangolin: &Pangolin, stop: &AtomicBool) -> Result<Vec<String>, String> {
    let domains = Api::open(pangolin, stop)?.domains()?;
    if domains.is_empty() {
        return Err("Pangolin has no verified domain; add one in Pangolin first".to_owned());
    }
    Ok(domains.into_iter().map(|d| d.base).collect())
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
    setting.picked = selected
        .and_then(|value| setting.choices.iter().position(|(name, _)| *name == value))
        .or_else(|| (setting.choices.len() == 1).then_some(0))
        .into_iter()
        .collect();
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
            || current_site["online"] != true
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
    selected_domain: &str,
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
    let (domain_id, base_domain, domain) = full_domain(&catalog, &exposure.subdomain, selected_domain)?;
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
    let bypass = api.bypass(resource.as_ref().map(|r| r.id), &exposure.bypass_paths)?;
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
        domain_id,
        base_domain,
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

    fn target_unchanged(&self) -> bool {
        self.previous.as_ref().is_some_and(|target| target.body == self.body)
    }

    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "https://{} → Site {} (ID {}) → http://{}:{} · CT {} on {}",
                self.domain, self.site.name, self.site.id, self.address, self.body["port"], self.ctid, self.node
            ),
            match (&self.resource, &self.previous) {
                (None, _) => "Create the resource with Pangolin's default access policy (Pangolin login)".to_owned(),
                (Some(_), Some(target)) if self.target_unchanged() => {
                    format!("Backend target {} unchanged", target.id)
                }
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
        ];
        lines.extend(
            self.bypass
                .missing
                .iter()
                .map(|path| format!("Add rule: {path} bypasses Pangolin login")),
        );
        if self.bypass.enable {
            lines.push("Turn on rule evaluation for the resource".to_owned());
        }
        lines.push(
            "Other access policies of an existing resource, the container, disks and /home/app are preserved."
                .to_owned(),
        );
        lines
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
        let bypass = api.bypass(Some(resource), &plan.bypass_paths)?;
        if plan.resource.is_some() && bypass != plan.bypass {
            return Err("Pangolin rules changed since review; review again".to_owned());
        }
        if !plan.target_unchanged() {
            save_target(&api, plan, resource)?;
        }
        lines.push(format!(
            "https://{} routed through {} to {}:{}; container and /home/app preserved",
            plan.domain, plan.site.name, plan.address, plan.body["port"]
        ));
        for (path, priority) in bypass.missing.iter().zip(bypass.priority..) {
            let rule =
                json!({"action": "ACCEPT", "match": "PATH", "value": path, "priority": priority, "enabled": true});
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
        if api.bypass(Some(resource), &plan.bypass_paths)? != Bypass::default() {
            return Err("Saved Pangolin rules do not match the review; inspect the resource rules".to_owned());
        }
        Ok(lines)
    })
}

/// Create or update the backend target, then verify it reads back as reviewed.
fn save_target(api: &Api, plan: &Plan, resource: u64) -> Result<(), String> {
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
