//! Additive private HTTP publication. Existing objects are read and reused, never rewritten.
use super::*;
use std::collections::BTreeSet;
use std::net::{IpAddr, ToSocketAddrs};

pub(super) struct Plan {
    pub existing: Option<Value>,
    body: Value,
    sites: Vec<Site>,
    admin: Value,
    barrier: PathBuf,
}

pub(super) fn identifier(kind: &str, scope: &str, source: &Source, domain: &str) -> String {
    let mut hash = Sha256::new();
    for s in [scope, &source.repository, &source.manifest.id, domain] {
        hash.update((s.len() as u64).to_be_bytes());
        hash.update(s.as_bytes());
    }
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("hs-{kind}-{}", &hex[..32])
}

fn public_identity(row: &Value) -> Value {
    json!({"resourceId": row["resourceId"], "niceId": row["niceId"],
        "orgId": row["orgId"], "domainId": row["domainId"], "fullDomain": row["fullDomain"]})
}

/// A receipt proves that HomeStack created an otherwise incomplete public half.
pub(super) fn owns_public(path: &Path, row: &Value) -> Result<bool, String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("Cannot read Public creation receipt".into()),
    };
    let meta = file.metadata().map_err(|_| "Cannot inspect Public creation receipt")?;
    if !meta.is_file()
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.len() > 4096
    {
        return Err("Unsafe Public creation receipt".into());
    }
    let value: Value = serde_json::from_reader(file).map_err(|_| "Invalid Public creation receipt")?;
    Ok(value == public_identity(row))
}

pub(super) fn remember_public(path: &Path, row: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid Public receipt directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| "Cannot reserve Public creation receipt")?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "Cannot protect Public creation receipt")?;
    file.write_all(public_identity(row).to_string().as_bytes())
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| "Cannot save Public creation receipt")?;
    file.persist(path)
        .map_err(|_| "Cannot publish Public creation receipt")?;
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| "Cannot persist Public receipt directory")?;
    Ok(())
}

fn claims_hostname(claim: &str, domain: &str) -> bool {
    if claim.eq_ignore_ascii_case(domain) {
        return true;
    }
    if !claim.contains(['*', '?']) {
        return false;
    }
    let pattern = regex::escape(&claim.to_ascii_lowercase())
        .replace("\\*", ".*")
        .replace("\\?", ".");
    regex::Regex::new(&format!("^{pattern}$")).is_ok_and(|pattern| pattern.is_match(&domain.to_ascii_lowercase()))
}

pub(super) fn public_match(resources: &[Resource], domain: &str) -> Result<Option<Resource>, String> {
    let found: Vec<_> = resources
        .iter()
        .filter(|r| claims_hostname(&r.domain, domain))
        .collect();
    match found.as_slice() {
        [] => Ok(None),
        [r] if r.domain.eq_ignore_ascii_case(domain) => Ok(Some((*r).clone())),
        _ => Err(format!(
            "Multiple Public Resources use {domain}; select and repair the conflict explicitly"
        )),
    }
}

/// Local DNS may identify the same managed container by its IP or internal name.
/// Every returned address must agree; overlapping DNS sets alone are insufficient.
fn addresses(host: &str) -> Result<BTreeSet<IpAddr>, String> {
    if let Ok(ip) = host.parse() {
        return Ok(BTreeSet::from([ip]));
    }
    let addresses: BTreeSet<_> = (host, 0)
        .to_socket_addrs()
        .map_err(|_| format!("Cannot resolve backend {host}; its identity cannot be verified"))?
        .map(|a| a.ip())
        .collect();
    if addresses.is_empty() {
        return Err(format!("Backend {host} has no addresses"));
    }
    Ok(addresses)
}

fn same_destination(actual: &str, expected: &str) -> Result<bool, String> {
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(true);
    }
    Ok(addresses(actual)? == addresses(expected)?)
}

pub(super) fn verify_destination(hostname: &str, address: &str) -> Result<(), String> {
    if !same_destination(hostname, address)? {
        return Err(format!(
            "Local backend {hostname} does not resolve to the reviewed instance {address}; check local DNS before publication"
        ));
    }
    Ok(())
}

pub(super) fn validate_target(body: &Value, address: &str, port: u16) -> Result<(), String> {
    if !same_destination(&text(body, "ip")?, address)?
        || body["port"] != port
        || body["method"] != "http"
        || body["mode"] != "http"
        || body["enabled"] != true
        || ["path", "rewritePath"]
            .iter()
            .any(|f| body.get(*f).is_some_and(|v| !v.is_null() && v != ""))
    {
        return Err(format!(
            "Public backend {}:{} does not match http://{address}:{port}, or has path routing; repair explicitly before pairing",
            body["ip"], body["port"]
        ));
    }
    Ok(())
}

pub(super) fn public_snapshot(
    api: &Api<'_>,
    resource: &Resource,
    domain: &str,
    subdomain: &str,
) -> Result<Value, String> {
    let row = api.get(&format!("/v1/resource/{}", resource.id))?;
    if row["orgId"] != api.ctx.organization
        || row["domainId"] != domain
        || row["subdomain"] != subdomain
        || row["fullDomain"] != resource.domain
        || row["mode"] != "http"
        || row["ssl"] != true
        || row["enabled"] != true
    {
        return Err(format!(
            "Public Resource {} conflicts with the selected organization/domain or enabled HTTPS mode; repair explicitly",
            resource.id
        ));
    }
    // Preserve even manually managed policies. Their snapshots guard the CREATE of the private half.
    Ok(json!({"resource": row,
        "policies": api.get(&format!("/v1/resource/{}/policies", resource.id))?,
        "rules": api.get(&format!("/v1/resource/{}/rules?limit=1000", resource.id))?}))
}

fn sorted_ids(rows: &Value, field: &str) -> Result<Value, String> {
    let mut ids = rows
        .as_array()
        .ok_or("Unexpected Pangolin authorization listing")?
        .iter()
        .map(|r| r.get(field).cloned().ok_or("Missing Pangolin authorization identity"))
        .collect::<Result<Vec<_>, _>>()?;
    ids.sort_by_key(Value::to_string);
    Ok(json!(ids))
}

fn site_ids(row: &Value) -> Result<Vec<u64>, String> {
    let mut ids = row["siteIds"]
        .as_array()
        .ok_or("Private Resource listing omitted Site associations")?
        .iter()
        .map(|v| v.as_u64().filter(|id| *id > 0).ok_or("Invalid private Site ID"))
        .collect::<Result<Vec<_>, _>>()?;
    ids.sort_unstable();
    if ids.is_empty() || ids.windows(2).any(|p| p[0] == p[1]) {
        return Err("Private Resource has missing or duplicate Sites".into());
    }
    Ok(ids)
}

impl Api<'_> {
    fn admin(&self) -> Result<Value, String> {
        let rows = self.rows("roles")?;
        let admins: Vec<_> = rows.into_iter().filter(|r| r["isAdmin"] == true).collect();
        match admins.as_slice() {
            [role] if role["orgId"] == self.ctx.organization => {
                id(role, "roleId")?;
                Ok(
                    json!({"roleId": role["roleId"], "orgId": role["orgId"], "name": text(role, "name")?, "isAdmin": true}),
                )
            }
            _ => Err("Cannot resolve a unique Admin role in this organization; nothing is changed".into()),
        }
    }

    fn private_snapshot(&self, domain: &str) -> Result<Option<Value>, String> {
        let rows = self.rows("private-resources")?;
        let matches: Vec<_> = rows
            .iter()
            .filter(|r| {
                r["fullDomain"].as_str().is_some_and(|s| claims_hostname(s, domain))
                    || r["alias"].as_str().is_some_and(|s| claims_hostname(s, domain))
            })
            .collect();
        let row = match matches.as_slice() {
            [] => return Ok(None),
            [row] => row,
            _ => {
                return Err(format!(
                    "Multiple Private Resources claim {domain}; repair the conflict explicitly"
                ));
            }
        };
        let rid = id(row, "siteResourceId")?;
        let detail = self.get(&format!("/v1/private-resource/{rid}"))?;
        if detail["orgId"] != self.ctx.organization || id(&detail, "siteResourceId")? != rid {
            return Err("Private Resource organization/identity changed; review again".into());
        }
        let mut snapshot = Map::new();
        for key in [
            "siteResourceId",
            "orgId",
            "niceId",
            "name",
            "mode",
            "ssl",
            "scheme",
            "destination",
            "destinationPort",
            "enabled",
            "alias",
            "domainId",
            "subdomain",
            "fullDomain",
            "tcpPortRangeString",
            "udpPortRangeString",
            "disableIcmp",
            "authDaemonMode",
            "authDaemonPort",
            "pamMode",
            "status",
            "networkId",
            "aliasAddress",
            "requiresExitNodeConnection",
        ] {
            snapshot.insert(key.into(), detail.get(key).cloned().unwrap_or(Value::Null));
        }
        snapshot.insert("siteIds".into(), json!(site_ids(row)?));
        for (route, field, identity) in [
            ("roles", "roles", "roleId"),
            ("users", "users", "userId"),
            ("clients", "clients", "clientId"),
        ] {
            let data = self.get(&format!("/v1/private-resource/{rid}/{route}"))?;
            snapshot.insert(route.into(), sorted_ids(&data[field], identity)?);
        }
        Ok(Some(Value::Object(snapshot)))
    }
}

fn check_private(row: &Value, body: &Value, admin: &Value) -> Result<(), String> {
    for key in [
        "domainId",
        "subdomain",
        "mode",
        "ssl",
        "scheme",
        "destinationPort",
        "tcpPortRangeString",
        "udpPortRangeString",
        "disableIcmp",
        "authDaemonMode",
        "authDaemonPort",
        "pamMode",
    ] {
        if row[key] != body[key] {
            return Err(format!(
                "Private Resource {} has unexpected {key}: {} (expected {}); repair explicitly",
                row["siteResourceId"], row[key], body[key]
            ));
        }
    }
    if row["enabled"] != true
        || row["status"] != "approved"
        || !row["alias"].is_null()
        || !same_destination(&text(row, "destination")?, &text(body, "destination")?)?
        || row["siteIds"] != body["siteIds"]
        || row["roles"] != json!([admin["roleId"]])
        || row["users"] != json!([])
        || row["clients"] != json!([])
        || row["requiresExitNodeConnection"] == true
    {
        return Err(format!(
            "Private Resource {} conflicts with the backend, Site set, enabled state or Admin-only authorization; nothing is overwritten",
            row["siteResourceId"]
        ));
    }
    Ok(())
}

pub(super) fn review(
    api: &Api<'_>,
    catalog: &Catalog,
    source: &Source,
    publication: &Publication,
    domain: &str,
    domain_id: &str,
    target: &Value,
    public_site: &Site,
) -> Result<Plan, String> {
    let admin = api.admin()?;
    let existing = api.private_snapshot(domain)?;
    let names: Vec<String> = if !publication.private_sites.is_empty() {
        publication.private_sites.clone()
    } else if let Some(row) = &existing {
        site_ids(row)?
            .into_iter()
            .map(|id| {
                catalog
                    .sites
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.nice.clone())
                    .ok_or("An existing Private Resource Site is unavailable".to_owned())
            })
            .collect::<Result<_, _>>()?
    } else {
        vec![public_site.nice.clone()]
    };
    let mut sites = names
        .iter()
        .map(|n| {
            catalog
                .sites
                .iter()
                .find(|s| &s.nice == n)
                .cloned()
                .ok_or_else(|| format!("Private Site {n} is not online; review again"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    sites.sort_by_key(|s| s.id);
    sites.dedup_by_key(|s| s.id);
    if sites.is_empty() || !sites.iter().any(|s| s.online) {
        return Err("Select at least one online reachable Private Site".into());
    }
    if existing.is_none() && sites.iter().any(|s| !s.online) {
        return Err("A selected Private Site is offline; select only currently reachable routes for creation".into());
    }
    for site in &sites {
        api.validate(None, site)?;
    }
    let nice = identifier("private", &api.ctx.identity, source, domain);
    let body = json!({"name": source.manifest.pangolin.as_ref().ok_or("Missing exposure")?.name,
        "niceId": nice, "mode": "http", "ssl": true, "scheme": "http",
        "domainId": domain_id, "subdomain": publication.subdomain,
        // Omit alias: the API accepts a string, not null; it persists null when absent.
        "destination": target["ip"], "destinationPort": target["port"],
        "siteIds": sites.iter().map(|s| s.id).collect::<Vec<_>>(),
        // Pangolin inserts the organization's Admin role itself in the creation transaction.
        "roleIds": [], "userIds": [], "clientIds": [],
        "tcpPortRangeString": "443,80", "udpPortRangeString": "", "disableIcmp": true,
        "authDaemonMode": "site", "authDaemonPort": 22123, "pamMode": "passthrough"});
    if let Some(row) = &existing {
        check_private(row, &body, &admin)?;
    }
    let barrier = api.ctx.key.with_file_name(format!("{nice}.creation-pending"));
    if existing.is_none()
        && barrier
            .try_exists()
            .map_err(|_| "Cannot inspect private creation recovery marker")?
    {
        return Err(format!(
            "Private creation outcome is unresolved for {domain}; no matching resource is visible. Inspect Pangolin before explicitly removing recovery marker {}. No creation is retried",
            barrier.display()
        ));
    }
    Ok(Plan {
        existing,
        body,
        sites,
        admin,
        barrier,
    })
}

impl Plan {
    pub(super) fn lines(&self, domain: &str) -> Vec<String> {
        vec![
            format!(
                "Private Resource: {} · https://{domain}",
                if self.existing.is_some() { "KEEP" } else { "CREATE" }
            ),
            format!(
                "Private backend: http://{}:{}; Pangolin-managed HTTPS, no host alias",
                self.body["destination"].as_str().unwrap_or_default(),
                self.body["destinationPort"]
            ),
            format!(
                "Private Sites: {}",
                self.sites
                    .iter()
                    .map(|s| format!("{} (ID {})", s.name, s.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "Private authorization: existing {} role (ID {}); no direct user/device grants",
                self.admin["name"].as_str().unwrap_or("Admin"),
                self.admin["roleId"]
            ),
        ]
    }

    pub(super) fn validate(&self, api: &Api<'_>, catalog: &Catalog, plan: &super::Plan) -> Result<(), String> {
        if api.admin()? != self.admin || api.private_snapshot(&plan.domain)? != self.existing {
            return Err("Private Resource or Admin role changed since review; review again".into());
        }
        for site in &self.sites {
            if !catalog.sites.contains(site) {
                return Err("Private Site changed since review".into());
            }
            api.validate(None, site)?;
        }
        if let Some(row) = &self.existing {
            check_private(row, &self.body, &self.admin)?;
        }
        // Re-resolve internal DNS before mutation as it may have changed after review.
        verify_destination(&plan.backend, &plan.address)?;
        validate_target(
            &plan.body,
            &plan.address,
            plan.body["port"].as_u64().ok_or("Invalid target port")? as u16,
        )?;
        Ok(())
    }

    pub(super) fn apply(&self, api: &Api<'_>, plan: &super::Plan) -> Result<Vec<String>, String> {
        // Called again after Public creation: relevant observations must still match.
        self.validate(api, &api.catalog()?, plan)?;
        if self.existing.is_some() {
            return Ok(vec!["Private Resource unchanged".into()]);
        }
        let public = public_match(&api.catalog()?.resources, &plan.domain)?
            .ok_or("Public Resource disappeared; review again")?;
        let snapshot = public_snapshot(api, &public, &plan.domain_id, &plan.publication.subdomain)?;
        if let Some(reviewed) = plan.public_snapshot.as_ref().filter(|_| plan.keeps_public()) {
            if snapshot != *reviewed {
                return Err("Public Resource changed before private creation; review again".into());
            }
        } else if snapshot["resource"]["sso"] != true {
            return Err("The new Public Resource does not have SSO enabled; inspect its default policy".into());
        }
        let target = api
            .target(public.id)?
            .ok_or("Public target disappeared; review again")?;
        let target_matches = if plan.previous.is_some() {
            target.body == plan.body
        } else {
            plan.body
                .as_object()
                .is_some_and(|body| body.iter().all(|(k, v)| target.body.get(k) == Some(v)))
        };
        if !target_matches {
            return Err("Public target changed before private creation; review again".into());
        }
        // Exclusive durable barrier prevents repeated or concurrent PUTs, including after restart.
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&self.barrier)
            .map_err(
                |_| "Private creation already attempted or recovery marker cannot be reserved; refresh before retrying",
            )?;
        marker
            .write_all(b"Private Resource creation requested; inspect inventory before retrying.\n")
            .and_then(|_| marker.sync_all())
            .map_err(|_| "Cannot persist private creation recovery marker")?;
        std::fs::File::open(self.barrier.parent().ok_or("Invalid recovery directory")?)
            .map_err(|_| "Cannot open private recovery directory")?
            .sync_all()
            .map_err(|_| "Cannot persist private creation recovery directory")?;
        let path = format!("/v1/org/{}/private-resource", api.ctx.organization);
        let (status, response) = api
            .client()
            .request("PUT", &path, Some(&self.body.to_string()), Probe::Organization)
            .map_err(
                |_| "Private creation outcome unknown; refresh inventory. Creation will not be retried automatically",
            )?;
        let reply: Value = serde_json::from_slice(&response).unwrap_or_default();
        if ![200, 201].contains(&status) || reply["success"] != true || reply["error"] != false {
            if [400, 401, 403, 404, 409, 422, 429].contains(&status) {
                std::fs::remove_file(&self.barrier)
                    .map_err(|_| "Creation rejected but recovery marker remains; inspect before retrying")?;
            }
            return Err(if status == 403 {
                "Private creation denied (createSiteResource); use Step 7 bootstrap-authorized Merge, then review again"
                    .into()
            } else {
                format!("Private creation not confirmed (HTTP {status}); refresh inventory before retrying")
            });
        }
        let saved = api
            .private_snapshot(&plan.domain)?
            .ok_or("Private creation confirmed but resource is not visible; refresh inventory")?;
        if saved["siteResourceId"] != reply["data"]["siteResourceId"] || saved["niceId"] != self.body["niceId"] {
            return Err("Private creation identity differs from confirmation; inspect inventory".into());
        }
        check_private(&saved, &self.body, &self.admin)?;
        Ok(vec![format!(
            "Private HTTP Resource {} created for https://{}; Admin access, public configuration preserved",
            saved["siteResourceId"], plan.domain
        )])
    }
}
