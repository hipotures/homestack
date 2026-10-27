//! The commands of `homestack-deploy` (`docs/migration/phase-11.md`, 2.3).

use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hs_client::{Client, ClientConfig, Endpoint};
use hs_core::pve::Pve;
use hs_model::jobs::Secret;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::acme;
use crate::config::{DeployConfig, InstanceSpec};
use crate::dns::DnsApi;
use crate::node::{self, NewContainer};
use crate::recreate;

/// How long a new instance may take to answer over TLS.
const HEALTH_LIMIT: Duration = Duration::from_secs(15);
/// Pause between two `/health/live` probes while waiting for an instance.
const HEALTH_RETRY: Duration = Duration::from_millis(250);
/// Variables that carry the certificate; removed once the server stored it.
const TLS_VARIABLES: [&str; 2] = ["HOMESTACK_TLS_CERT_B64", "HOMESTACK_TLS_KEY_B64"];

fn read_secret(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|e| format!("{}: {}", path.display(), e.kind()))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

/// The Proxmox client with the deploy token.
pub fn deploy_client(config: &DeployConfig) -> Result<PveClient, String> {
    let token = PveToken::from_file(&config.pve.deploy_token_file).map_err(|e| e.to_string())?;
    if !token.id().ends_with("!deploy") {
        return Err(format!("{} is not the deploy token; refusing", token.id()));
    }
    let mut client = PveClientConfig::new(config.pve.endpoints.clone(), token);
    if let Some(path) = &config.pve.ca_file {
        client.extra_ca_pem =
            Some(std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?);
    }
    PveClient::new(client).map_err(|e| e.to_string())
}

/// A client of `instances`, each reached at its address with its name.
pub fn instances_client(config: &DeployConfig, instances: &[&InstanceSpec], api_token: &str) -> Result<Client, String> {
    let endpoints = instances
        .iter()
        .map(|instance| {
            Ok(Endpoint {
                url: config.url(instance)?,
                address: Some(SocketAddr::from((instance.address, 443))),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut client = ClientConfig::new(endpoints, api_token.to_owned());
    if let Some(path) = &config.acme.ca_file {
        client.extra_ca_pem =
            Some(std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?);
    }
    Client::new(client).map_err(|e| e.to_string())
}

/// The stored certificate, if it covers `*.<domain>` and has not expired.
fn certificate(config: &DeployConfig) -> Result<acme::Stored, String> {
    let stored = acme::stored(&config.acme.dir)?.ok_or_else(|| {
        format!(
            "no certificate in {}: run `homestack-deploy cert` first",
            config.acme.dir.display()
        )
    })?;
    if !stored.names.iter().any(|name| *name == format!("*.{}", config.domain)) {
        return Err(format!(
            "the stored certificate is for {:?}, not *.{}",
            stored.names, config.domain
        ));
    }
    if stored.not_after <= now() {
        return Err("the stored certificate has expired: run `homestack-deploy cert`".to_owned());
    }
    Ok(stored)
}

async fn timed<T>(
    instance: &str,
    stage: &str,
    say: &impl Fn(&str),
    work: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let started = Instant::now();
    say(&format!("  {instance}: {stage} started"));
    let result = work.await;
    let outcome = if result.is_ok() { "completed" } else { "failed" };
    say(&format!(
        "  {instance}: {stage} {outcome} in {} ms",
        started.elapsed().as_millis()
    ));
    result
}

fn health_error(error: &hs_client::ClientError) -> String {
    match error {
        hs_client::ClientError::Api { status, code, .. } => format!("HTTP {status}, code={code}"),
        _ => error.to_string(),
    }
}

async fn wait_health(
    client: &Client,
    instance: &str,
    phase: &str,
    limit: Duration,
    say: &impl Fn(&str),
) -> Result<(), String> {
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + limit;
    let mut reason = "request timed out".to_owned();
    say(&format!(
        "  {instance}: waiting for HTTPS {phase} (up to {} s)",
        limit.as_secs()
    ));
    loop {
        match tokio::time::timeout_at(deadline, client.get_on(0, "/health/live")).await {
            Ok(Ok(_)) => {
                say(&format!(
                    "  {instance}: HTTPS ready {phase} ({} ms)",
                    started.elapsed().as_millis()
                ));
                return Ok(());
            }
            Ok(Err(error)) => reason = health_error(&error),
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "{instance}: HTTPS health {phase}: no answer over TLS within {} s: {reason}",
                limit.as_secs()
            ));
        }
        tokio::time::sleep_until((tokio::time::Instant::now() + HEALTH_RETRY).min(deadline)).await;
    }
}

/// Authenticated API initialization, independent of LXC creation/replacement.
/// Secrets are read afresh at review/apply and never included in a plan or Debug.
pub struct ApiBootstrap {
    config: DeployConfig,
    stored: acme::Stored,
    api_token: Secret,
    pve_token: Secret,
    server_toml: String,
    verifier: rustls_platform_verifier::Verifier,
}

impl ApiBootstrap {
    /// Validate the existing desktop inputs without contacting or changing a guest.
    pub fn load(config: &DeployConfig) -> Result<Self, String> {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
        let stored = certificate(config)?;
        let certs = CertificateDer::pem_slice_iter(stored.cert.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "API bootstrap: invalid certificate chain")?;
        let key = PrivateKeyDer::from_pem_slice(stored.key.as_bytes())
            .map_err(|_| "API bootstrap: invalid TLS private key")?;
        rustls::sign::CertifiedKey::from_der(certs, key, &rustls::crypto::aws_lc_rs::default_provider())
            .and_then(|key| key.keys_match())
            .map_err(|_| "API bootstrap: certificate and private key do not match")?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(stored.cert.as_bytes())
            .map_err(|_| "API bootstrap: invalid certificate")?;
        if !pem
            .parse_x509()
            .map_err(|_| "API bootstrap: invalid certificate")?
            .validity()
            .is_valid()
        {
            return Err("API bootstrap: certificate is not currently valid".to_owned());
        }
        let api_token = read_secret(&config.server.api_token_file)?;
        if api_token.is_empty() || api_token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("API bootstrap: invalid or empty API bearer token".to_owned());
        }
        let pve_token = read_secret(&config.server.pve_token_file)?;
        let token = PveToken::parse(&pve_token).map_err(|_| "API bootstrap: invalid PVE token")?;
        PveClient::new(PveClientConfig::new(config.pve.endpoints.clone(), token))
            .map_err(|_| "API bootstrap: invalid PVE endpoint or token settings")?;
        let valid_pool = |name: &str| {
            let levels: Vec<_> = name.split('/').collect();
            levels.len() <= 3
                && levels.iter().all(|level| {
                    !level.is_empty() && level.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
                })
        };
        if !valid_pool(&config.server.pool)
            || !valid_pool(&config.server.gold_pool)
            || config.server.pool == config.server.gold_pool
        {
            return Err("API bootstrap: invalid or identical workspace and gold pool settings".to_owned());
        }
        let roots = if let Some(path) = &config.acme.ca_file {
            let pem = std::fs::read(path).map_err(|_| "API bootstrap: cannot read API CA certificates")?;
            let roots = CertificateDer::pem_slice_iter(&pem)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "API bootstrap: invalid API CA certificates")?;
            if roots.is_empty() {
                return Err("API bootstrap: API CA file contains no certificates".to_owned());
            }
            roots
        } else {
            Vec::new()
        };
        let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(
            roots,
            std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .map_err(|_| "API bootstrap: cannot load certificate trust roots")?;
        let server_toml = std::fs::read_to_string(&config.server.config_file)
            .map_err(|e| format!("API bootstrap: cannot read server.toml: {}", e.kind()))?;
        if server_toml.len() > 256 * 1024 {
            return Err("API bootstrap: server.toml exceeds 256 KiB".to_owned());
        }
        hs_model::config::Config::parse(&server_toml).map_err(|_| "API bootstrap: invalid server.toml")?;
        Ok(Self {
            config: config.clone(),
            stored,
            api_token: Secret::new(api_token),
            pve_token: Secret::new(pve_token),
            server_toml,
            verifier,
        })
    }

    /// Validate TLS client settings and the chosen instance hostname/address.
    pub fn validate_instance(&self, instance: &InstanceSpec) -> Result<(), String> {
        use rustls::{
            client::danger::ServerCertVerifier,
            pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject},
        };
        let hostname = format!("{}.{}", instance.name, self.config.domain);
        let name = ServerName::try_from(hostname).map_err(|_| "API bootstrap: invalid certificate hostname")?;
        let chain = CertificateDer::pem_slice_iter(self.stored.cert.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "API bootstrap: invalid certificate chain")?;
        let (leaf, intermediates) = chain.split_first().ok_or("API bootstrap: certificate chain is empty")?;
        self.verifier
            .verify_server_cert(leaf, intermediates, &name, &[], UnixTime::now())
            .map_err(
                |_| "API bootstrap: certificate chain or hostname is not trusted by the configured HTTPS client",
            )?;
        instances_client(&self.config, &[instance], self.api_token.expose()).map(|_| ())
    }

    fn token_hash(&self) -> String {
        Sha256::digest(self.api_token.expose().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn endpoints(&self) -> String {
        self.config
            .pve
            .endpoints
            .iter()
            .map(|url| url.as_str().trim_end_matches('/'))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Initialize or resume an already-created CT. Never creates, replaces,
    /// deletes or attaches storage, and never changes ownership descriptions.
    pub async fn initialize(
        &self,
        pve: &impl Pve,
        instance: &InstanceSpec,
        managed: bool,
        say: &impl Fn(&str),
    ) -> Result<(), String> {
        self.validate_instance(instance)?;
        let mut env = vec![
            ("HOMESTACK_INSTANCE".to_owned(), instance.name.clone()),
            ("HOMESTACK_API_TOKEN_SHA256".to_owned(), self.token_hash()),
            ("HOMESTACK_PVE_ENDPOINTS".to_owned(), self.endpoints()),
            ("HOMESTACK_PVE_POOL".to_owned(), self.config.server.pool.clone()),
            (
                "HOMESTACK_PVE_GOLD_POOL".to_owned(),
                self.config.server.gold_pool.clone(),
            ),
        ];
        if let Some(url) = &self.config.server.prometheus_url {
            env.push(("HOMESTACK_PROMETHEUS_URL".to_owned(), url.to_string()));
        }
        if managed {
            env.push(("HOMESTACK_MANAGED_HOME".to_owned(), "1".to_owned()));
        }
        let status = pve
            .get(
                &format!("/nodes/{}/lxc/{}/status/current", instance.node, instance.ctid),
                &[],
            )
            .await
            .map_err(|_| "API bootstrap: cannot read container power state")?;
        if managed {
            let cfg = pve
                .get(&format!("/nodes/{}/lxc/{}/config", instance.node, instance.ctid), &[])
                .await
                .map_err(|_| "API bootstrap: cannot read environment")?;
            let current = node::parse_env(cfg["env"].as_str().unwrap_or_default());
            let conflicting = current.iter().any(|(key, value)| {
                env.iter()
                    .find(|(expected, _)| expected == key)
                    .is_some_and(|(_, expected)| expected != value)
                    || (key.starts_with("HOMESTACK_")
                        && !TLS_VARIABLES.contains(&key.as_str())
                        && !env.iter().any(|(expected, _)| expected == key))
            });
            if conflicting || (status["status"] == "running" && env.iter().any(|entry| !current.contains(entry))) {
                return Err(
                    "API bootstrap: container settings differ; refusing to change its identity or state paths"
                        .to_owned(),
                );
            }
        }
        match status["status"].as_str() {
            Some("stopped") => {
                let b64 = |value: &str| base64::engine::general_purpose::STANDARD.encode(value);
                env.extend([
                    (TLS_VARIABLES[0].to_owned(), b64(&self.stored.cert)),
                    (TLS_VARIABLES[1].to_owned(), b64(&self.stored.key)),
                ]);
                timed(&instance.name, "set bootstrap environment", say, async {
                    node::set_env(pve, &instance.node, instance.ctid, &env, &[])
                        .await
                        .map_err(|_| "API bootstrap: could not set bootstrap environment".to_owned())
                })
                .await?;
                timed(
                    &instance.name,
                    "Proxmox start task",
                    say,
                    node::power(pve, &instance.node, instance.ctid, "start"),
                )
                .await?;
            }
            Some("running") if managed => {}
            _ => {
                return Err(
                    "API bootstrap: expected a stopped container, or a running managed initialization".to_owned(),
                );
            }
        }
        let client = instances_client(&self.config, &[instance], self.api_token.expose())?;
        wait_health(&client, &instance.name, "after start", HEALTH_LIMIT, say).await?;
        timed(&instance.name, "remove TLS bootstrap environment", say, async {
            node::set_env(pve, &instance.node, instance.ctid, &[], &TLS_VARIABLES)
                .await
                .map_err(|_| "API bootstrap: could not remove TLS bootstrap environment".to_owned())
        })
        .await?;
        if node::env_pending(pve, &instance.node, instance.ctid).await? {
            timed(
                &instance.name,
                "Proxmox reboot task",
                say,
                node::power(pve, &instance.node, instance.ctid, "reboot"),
            )
            .await?;
            wait_health(&client, &instance.name, "after reboot", HEALTH_LIMIT, say).await?;
        }
        if node::env_keys(pve, &instance.node, instance.ctid)
            .await?
            .iter()
            .any(|key| TLS_VARIABLES.contains(&key.as_str()))
        {
            return Err("API bootstrap: TLS bootstrap environment is still present".to_owned());
        }
        configure(&client, instance, self.pve_token.expose(), &self.server_toml, say).await?;
        if managed {
            timed(&instance.name, "verify persistent state after reboot", say, async {
                node::power(pve, &instance.node, instance.ctid, "reboot").await?;
                wait_health(&client, &instance.name, "persistent state", HEALTH_LIMIT, say).await?;
                let expected = hs_model::config::config_version(&self.server_toml);
                if client
                    .config_version_on(0)
                    .await
                    .map_err(|e| health_error(&e))?
                    .as_deref()
                    != Some(&expected)
                {
                    return Err("API bootstrap: configuration did not survive reboot".to_owned());
                }
                client
                    .get_on(0, "/v1/pve/version")
                    .await
                    .map_err(|e| format!("API bootstrap: stored PVE credentials: {}", health_error(&e)))?;
                let started = Instant::now();
                loop {
                    let report = client
                        .get_on(0, "/.well-known/homestack/status")
                        .await
                        .map_err(|e| health_error(&e))?;
                    if report["schema"] == 2
                        && report["sample_age_seconds"].as_u64().is_some_and(|age| age <= 30)
                        && report["filesystems"].as_array().is_some_and(|mounts| {
                            mounts
                                .iter()
                                .any(|m| m["mount"] == "/home/app" && m["mounted"] == true && m["error"].is_null())
                        })
                    {
                        break;
                    }
                    if started.elapsed() >= Duration::from_secs(15) {
                        return Err("API bootstrap: guest did not confirm a mounted /home/app".to_owned());
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }
}

/// `cert`: obtain or renew `*.<domain>`; with `push`, send it to every
/// running instance.
pub async fn cert(config: &DeployConfig, force: bool, push: bool, say: &impl Fn(&str)) -> Result<(), String> {
    let current = acme::stored(&config.acme.dir)?;
    let stored = match current {
        Some(stored) if !force && acme::current(&stored, &config.domain, now()) => {
            say(&format!(
                "*.{} is valid for {} more days; nothing to renew",
                config.domain,
                (stored.not_after - now()) / 86_400
            ));
            stored
        }
        _ => {
            say(&format!("obtaining *.{} from {}", config.domain, config.acme.directory));
            let dns = DnsApi::connect(&config.acme).await?;
            let stored = acme::obtain(
                &config.acme.directory,
                &config.acme.dir,
                &config.domain,
                &dns,
                &config.acme.zone,
                say,
            )
            .await?;
            say(&format!(
                "certificate for {:?} stored in {}, valid for {} days",
                stored.names,
                config.acme.dir.display(),
                (stored.not_after - now()) / 86_400
            ));
            stored
        }
    };
    if push {
        let api_token = read_secret(&config.server.api_token_file)?;
        let body = json!({"cert": stored.cert, "key": stored.key});
        for instance in &config.instances {
            let client = instances_client(config, &[instance], &api_token)?;
            match client.put_json_on(0, "/v1/secrets/tls", &body).await {
                Ok(_) => say(&format!("{}: certificate loaded", instance.name)),
                Err(error) => say(&format!("{}: not updated: {error}", instance.name)),
            }
        }
    }
    Ok(())
}

/// `apply`: (re)create every selected instance, one at a time, and check
/// that all instances hold the same configuration.
pub async fn apply(config: &DeployConfig, only: &[String], say: &impl Fn(&str)) -> Result<(), String> {
    let selected = config.selected(only)?;
    let bootstrap = ApiBootstrap::load(config)?;
    let stored = &bootstrap.stored;
    let pve = deploy_client(config)?;
    let api_token = bootstrap.api_token.expose();
    let pve_token = bootstrap.pve_token.expose();
    let server_toml = &bootstrap.server_toml;
    say("using the stored TLS certificate; apply does not request or renew a certificate");
    let hash = bootstrap.token_hash();
    let endpoints = bootstrap.endpoints();

    // What the container is made with, in its description: the same server,
    // image, certificate and settings keep the container.
    let settings: String = Sha256::digest(
        format!(
            "{hash}\n{endpoints}\n{}\n{}",
            config.server.pool, config.server.gold_pool
        )
        .as_bytes(),
    )
    .iter()
    .take(6)
    .map(|b| format!("{b:02x}"))
    .collect();
    for &instance in &selected {
        let started = Instant::now();
        say(&format!(
            "{}: CT {} on {} ({}, {})",
            instance.name, instance.ctid, instance.node, instance.address, instance.storage
        ));
        let mut net0 = format!(
            "name=eth0,bridge={},ip={}/{},gw={}",
            config.bridge, instance.address, config.cidr, config.gateway
        );
        let description = format!(
            "HomeStack server {}.{}\nimage {}\ncertificate {}\nsettings {settings}\nmade by homestack-deploy",
            instance.name, config.domain, config.image, stored.not_after
        );
        if let Some(found) = node::container(&pve, instance.ctid).await? {
            let server = format!("{}.{}", instance.name, config.domain);
            if found.node != instance.node
                || node::made_from(&pve, &found.node, instance.ctid, &server)
                    .await?
                    .is_none()
            {
                return Err(format!(
                    "CT {}: legacy API ownership does not match {server}; nothing is changed",
                    instance.ctid
                ));
            }
            let same = found.node == instance.node
                && found.status == "running"
                && node::unchanged(&pve, &instance.node, instance.ctid, &description, &net0, say).await?;
            if same {
                say("  the same image, certificate and address: only the configuration is sent");
                if node::ensure_tag(&pve, &instance.node, instance.ctid, say).await? {
                    say(&format!("  tagged {}", node::SERVER_TAG));
                }
                let client = instances_client(config, &[instance], api_token)?;
                wait_health(&client, &instance.name, "existing container", HEALTH_LIMIT, say).await?;
                configure(&client, instance, pve_token, server_toml, say).await?;
                say(&format!(
                    "  {}: instance apply completed in {} ms",
                    instance.name,
                    started.elapsed().as_millis()
                ));
                continue;
            }
            if let Some(mac) = node::hwaddr(&pve, &found.node, instance.ctid).await? {
                say(&format!("  keeping MAC address {mac}"));
                net0.push_str(&format!(",hwaddr={mac}"));
            }
        }
        let ostemplate = timed(
            &instance.name,
            "ensure image",
            say,
            node::ensure_image(&pve, &instance.node, &config.template_storage, &config.image, say),
        )
        .await?;
        let spec = NewContainer {
            searchdomain: None,
            swap_mib: 0,
            node: &instance.node,
            ctid: instance.ctid,
            hostname: &instance.name,
            ostemplate: &ostemplate,
            pool: &config.pool,
            storage: &instance.storage,
            disk_mib: config.disk_gib * 1024,
            memory_mib: config.memory_mib,
            cores: config.cores,
            net0: Some(net0),
            onboot: true,
            nameserver: config.nameserver.map(|ip| ip.to_string()),
            description,
            tag: Some(node::SERVER_TAG),
            mp0: None,
        };
        timed(
            &instance.name,
            "replace container",
            say,
            recreate::replace(
                &pve,
                &recreate::Replace {
                    spec,
                    graceful: false,
                    keep: &[],
                    note: "",
                },
                say,
            ),
        )
        .await?;
        bootstrap.initialize(&pve, instance, false, say).await?;
        say(&format!(
            "  {}: instance apply completed in {} ms",
            instance.name,
            started.elapsed().as_millis()
        ));
    }
    timed(
        "deployment",
        "configuration consistency check",
        say,
        check_versions_for(config, &selected, api_token, say),
    )
    .await?;
    let current = node::image_file(&config.image)?;
    let mut nodes: Vec<&str> = selected.iter().map(|instance| instance.node.as_str()).collect();
    nodes.sort_unstable();
    nodes.dedup();
    for name in nodes {
        node::remove_old_images(&pve, name, &config.template_storage, node::SERVER_IMAGES, &current, say).await;
    }
    Ok(())
}

/// The image each instance's container was made from, by instance name;
/// `None` for a missing container or one `apply` did not make.
pub async fn images(config: &DeployConfig) -> Result<Vec<(String, Option<String>)>, String> {
    let pve = deploy_client(config)?;
    let mut found = Vec::new();
    for instance in &config.instances {
        let image = match node::container(&pve, instance.ctid).await? {
            Some(container) => {
                let server = format!("{}.{}", instance.name, config.domain);
                node::made_from(&pve, &container.node, instance.ctid, &server).await?
            }
            None => None,
        };
        found.push((instance.name.clone(), image));
    }
    Ok(found)
}

/// Every instance in `only` (or all when empty) holds the same configuration version.
pub async fn check_selected_versions(
    config: &DeployConfig,
    only: &[String],
    say: &impl Fn(&str),
) -> Result<(), String> {
    let selected = config.selected(only)?;
    let api_token = read_secret(&config.server.api_token_file)?;
    timed(
        "deployment",
        "configuration consistency check",
        say,
        check_versions_for(config, &selected, &api_token, say),
    )
    .await
}

/// Every instance holds the same configuration version.
pub async fn check_versions(config: &DeployConfig, api_token: &str, say: &impl Fn(&str)) -> Result<(), String> {
    let all: Vec<&InstanceSpec> = config.instances.iter().collect();
    check_versions_for(config, &all, api_token, say).await
}

async fn check_versions_for(
    config: &DeployConfig,
    instances: &[&InstanceSpec],
    api_token: &str,
    say: &impl Fn(&str),
) -> Result<(), String> {
    let client = instances_client(config, instances, api_token)?;
    let mut versions = std::collections::BTreeSet::new();
    let mut problems = Vec::new();
    for (index, url) in client.urls().enumerate() {
        let version = timed(url.as_str(), "GET /v1/config/version", say, async {
            client.config_version_on(index).await.map_err(|error| error.to_string())
        })
        .await;
        match version {
            Ok(Some(version)) => {
                say(&format!("{url}: configuration {}", &version[..12.min(version.len())]));
                versions.insert(version);
            }
            Ok(None) => problems.push(format!("{url}: no configuration")),
            Err(error) => problems.push(format!("{url}: {error}")),
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    if versions.len() != 1 {
        return Err(format!("the instances hold {} configuration versions", versions.len()));
    }
    if instances.len() > 1 {
        say("all instances hold the same configuration");
    }
    Ok(())
}

/// `status`: each instance's container and what it answers.
pub async fn status(config: &DeployConfig, only: &[String], say: &impl Fn(&str)) -> Result<(), String> {
    let pve = deploy_client(config)?;
    let api_token = read_secret(&config.server.api_token_file)?;
    if let Some(stored) = acme::stored(&config.acme.dir)? {
        say(&format!(
            "certificate {:?}: {} days left",
            stored.names,
            (stored.not_after - now()) / 86_400
        ));
    }
    let current = node::image_file(&config.image)?;
    let mut nodes: Vec<&str> = config.instances.iter().map(|instance| instance.node.as_str()).collect();
    nodes.sort_unstable();
    nodes.dedup();
    for name in nodes {
        let old = node::old_images(&pve, name, &config.template_storage, node::SERVER_IMAGES, &current).await?;
        if !old.is_empty() {
            say(&format!(
                "{name}: old server images (the next apply removes them): {}",
                old.join(" ")
            ));
        }
    }
    for instance in config.selected(only)? {
        let Some(found) = node::container(&pve, instance.ctid).await? else {
            say(&format!("{}: CT {} does not exist", instance.name, instance.ctid));
            continue;
        };
        let client = instances_client(config, &[instance], &api_token)?;
        let version = match client.config_version_on(0).await {
            Ok(Some(version)) => version[..12.min(version.len())].to_owned(),
            Ok(None) => "none".to_owned(),
            Err(error) => format!("unreachable ({error})"),
        };
        say(&format!(
            "{}: CT {} on {} {}, configuration {version}",
            instance.name, instance.ctid, found.node, found.status
        ));
    }
    Ok(())
}

/// `remove`: stop and destroy the selected containers.
pub async fn remove(config: &DeployConfig, only: &[String], say: &impl Fn(&str)) -> Result<(), String> {
    let pve = deploy_client(config)?;
    for instance in config.selected(only)? {
        match node::container(&pve, instance.ctid).await? {
            Some(found) => {
                say(&format!("{}: CT {}", instance.name, instance.ctid));
                node::destroy(&pve, &found, instance.ctid, say).await?;
            }
            None => say(&format!("{}: CT {} does not exist", instance.name, instance.ctid)),
        }
    }
    Ok(())
}

/// Send the Proxmox token and `server.toml` to one instance and verify its
/// active configuration version immediately before proceeding to the next node.
async fn configure(
    client: &Client,
    instance: &InstanceSpec,
    pve_token: &str,
    server_toml: &str,
    say: &impl Fn(&str),
) -> Result<(), String> {
    timed(&instance.name, "upload PVE token", say, async {
        client
            .put_text_on(0, "/v1/secrets/pve-token", pve_token, "text/plain")
            .await
            .map_err(|error| format!("{}: Proxmox token: {}", instance.name, health_error(&error)))
    })
    .await?;
    let version = timed(&instance.name, "upload server.toml", say, async {
        client
            .configure_on(0, server_toml)
            .await
            .map_err(|error| format!("{}: server.toml: {}", instance.name, health_error(&error)))
    })
    .await?;
    let reported = timed(&instance.name, "verify configuration version", say, async {
        client
            .config_version_on(0)
            .await
            .map_err(|error| format!("{}: config version: {}", instance.name, health_error(&error)))
    })
    .await?;
    if reported.as_deref() != Some(version.as_str()) {
        return Err(format!(
            "{}: reported configuration {:?} does not match uploaded {}",
            instance.name,
            reported,
            &version[..12.min(version.len())]
        ));
    }
    say(&format!("  ready, configuration {}", &version[..12.min(version.len())]));
    Ok(())
}
