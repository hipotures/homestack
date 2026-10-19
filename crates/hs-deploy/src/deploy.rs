//! The commands of `homestack-deploy` (`docs/migration/phase-11.md`, 2.3).

use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hs_client::{Client, ClientConfig, Endpoint};
use hs_pve::{PveClient, PveClientConfig, PveToken};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::acme;
use crate::config::{DeployConfig, InstanceSpec};
use crate::dns::DnsApi;
use crate::node::{self, NewContainer};

/// How long a new instance may take to answer over TLS.
const HEALTH_LIMIT: Duration = Duration::from_secs(180);
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
    let mut attempt = 0_u64;
    say(&format!(
        "  {instance}: HTTPS health {phase} started (GET /health/live, limit {} ms, retry 2000 ms)",
        limit.as_millis()
    ));
    loop {
        attempt += 1;
        let probe_started = Instant::now();
        say(&format!(
            "  {instance}: HTTPS health {phase} attempt {attempt} started (elapsed {} ms)",
            started.elapsed().as_millis()
        ));
        match client.get_on(0, "/health/live").await {
            Ok(_) => {
                say(&format!(
                    "  {instance}: HTTPS health {phase} attempt {attempt} succeeded after {} ms (elapsed {} ms)",
                    probe_started.elapsed().as_millis(),
                    started.elapsed().as_millis()
                ));
                return Ok(());
            }
            Err(error) => {
                let reason = health_error(&error);
                say(&format!(
                    "  {instance}: HTTPS health {phase} attempt {attempt} failed after {} ms (elapsed {} ms): {reason}",
                    probe_started.elapsed().as_millis(),
                    started.elapsed().as_millis()
                ));
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!("no answer over TLS within {} s: {reason}", limit.as_secs()));
                }
                say(&format!("  {instance}: HTTPS health {phase} retrying in 2000 ms"));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
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
    let stored = certificate(config)?;
    let pve = deploy_client(config)?;
    let api_token = read_secret(&config.server.api_token_file)?;
    let pve_token = read_secret(&config.server.pve_token_file)?;
    PveToken::parse(&pve_token).map_err(|e| format!("{}: {e}", config.server.pve_token_file.display()))?;
    let server_toml = std::fs::read_to_string(&config.server.config_file)
        .map_err(|e| format!("{}: {}", config.server.config_file.display(), e.kind()))?;
    say("using the stored TLS certificate; apply does not request or renew a certificate");
    let hash: String = Sha256::digest(api_token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
    let endpoints = config
        .pve
        .endpoints
        .iter()
        .map(|url| url.as_str().trim_end_matches('/').to_owned())
        .collect::<Vec<_>>()
        .join(",");

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
            let same = found.node == instance.node
                && found.status == "running"
                && node::unchanged(&pve, &instance.node, instance.ctid, &description, &net0, say).await?;
            if same {
                say("  the same image, certificate and address: only the configuration is sent");
                if node::ensure_tag(&pve, &instance.node, instance.ctid, say).await? {
                    say(&format!("  tagged {}", node::SERVER_TAG));
                }
                let client = instances_client(config, &[instance], &api_token)?;
                wait_health(&client, &instance.name, "existing container", HEALTH_LIMIT, say).await?;
                configure(&client, instance, &pve_token, &server_toml, say).await?;
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
            timed(
                &instance.name,
                "stop and destroy",
                say,
                node::destroy(&pve, &found, instance.ctid, say),
            )
            .await?;
        }
        let ostemplate = timed(
            &instance.name,
            "ensure image",
            say,
            node::ensure_image(&pve, &instance.node, &config.template_storage, &config.image, say),
        )
        .await?;
        let spec = NewContainer {
            node: &instance.node,
            ctid: instance.ctid,
            hostname: &instance.name,
            ostemplate: &ostemplate,
            pool: &config.pool,
            storage: &instance.storage,
            disk_gib: config.disk_gib,
            memory_mib: config.memory_mib,
            cores: config.cores,
            net0,
            nameserver: config.nameserver.map(|ip| ip.to_string()),
            description,
        };
        timed(
            &instance.name,
            "create container and tag",
            say,
            node::create(&pve, &spec, say),
        )
        .await?;
        let env = [
            ("HOMESTACK_INSTANCE", instance.name.clone()),
            ("HOMESTACK_API_TOKEN_SHA256", hash.clone()),
            ("HOMESTACK_PVE_ENDPOINTS", endpoints.clone()),
            ("HOMESTACK_PVE_POOL", config.server.pool.clone()),
            ("HOMESTACK_PVE_GOLD_POOL", config.server.gold_pool.clone()),
            (TLS_VARIABLES[0], b64(&stored.cert)),
            (TLS_VARIABLES[1], b64(&stored.key)),
        ]
        .map(|(key, value)| (key.to_owned(), value));
        timed(
            &instance.name,
            "set bootstrap environment",
            say,
            node::set_env(&pve, &instance.node, instance.ctid, &env, &[]),
        )
        .await?;
        say("  starting");
        timed(
            &instance.name,
            "Proxmox start task",
            say,
            node::power(&pve, &instance.node, instance.ctid, "start"),
        )
        .await?;
        let client = instances_client(config, &[instance], &api_token)?;
        wait_health(&client, &instance.name, "after start", HEALTH_LIMIT, say).await?;
        say("  answers over TLS; removing the certificate from the container's configuration");
        timed(
            &instance.name,
            "remove TLS bootstrap environment",
            say,
            node::set_env(&pve, &instance.node, instance.ctid, &[], &TLS_VARIABLES),
        )
        .await?;
        if node::env_pending(&pve, &instance.node, instance.ctid).await? {
            say("  the change waits for a restart; restarting");
            timed(
                &instance.name,
                "Proxmox reboot task",
                say,
                node::power(&pve, &instance.node, instance.ctid, "reboot"),
            )
            .await?;
            wait_health(&client, &instance.name, "after reboot", HEALTH_LIMIT, say).await?;
        }
        let keys = node::env_keys(&pve, &instance.node, instance.ctid).await?;
        if keys.iter().any(|key| TLS_VARIABLES.contains(&key.as_str())) {
            return Err(format!(
                "{}: the certificate is still in the container's configuration",
                instance.name
            ));
        }
        configure(&client, instance, &pve_token, &server_toml, say).await?;
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
        check_versions_for(config, &selected, &api_token, say),
    )
    .await
}

/// The image each instance's container was made from, by instance name;
/// `None` for a missing container or one `apply` did not make.
pub async fn images(config: &DeployConfig) -> Result<Vec<(String, Option<String>)>, String> {
    let pve = deploy_client(config)?;
    let mut found = Vec::new();
    for instance in &config.instances {
        let image = match node::container(&pve, instance.ctid).await? {
            Some(container) => node::made_from(&pve, &container.node, instance.ctid).await?,
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
        // The deploy token may not remove them (decisions.md, phase 11).
        let old = node::old_images(&pve, name, &config.template_storage, &current).await?;
        if !old.is_empty() {
            say(&format!(
                "{name}: old server images (root removes them): {}",
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
            .map_err(|error| format!("{}: Proxmox token: {error}", instance.name))
    })
    .await?;
    let version = timed(&instance.name, "upload server.toml", say, async {
        client
            .configure_on(0, server_toml)
            .await
            .map_err(|error| format!("{}: server.toml: {error}", instance.name))
    })
    .await?;
    let reported = timed(&instance.name, "verify configuration version", say, async {
        client
            .config_version_on(0)
            .await
            .map_err(|error| format!("{}: config version: {error}", instance.name))
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};

    use super::*;

    type Error = Box<dyn std::error::Error>;

    fn health_client(
        responses: Vec<(u16, &'static str)>,
    ) -> Result<(Client, std::thread::JoinHandle<std::io::Result<()>>), Error> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut connection, _) = listener.accept()?;
                connection.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let count = connection.read(&mut buffer)?;
                    if count == 0 {
                        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                write!(
                    connection,
                    "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            Ok(())
        });
        let endpoint = Endpoint::from(format!("http://{address}").parse::<url::Url>()?);
        let mut config = ClientConfig::new(vec![endpoint], "dummy-client-token-not-for-logging".to_owned());
        config.request_timeout = Duration::from_secs(1);
        Ok((Client::new(config)?, server))
    }

    #[tokio::test]
    async fn stage_timing_reports_success_and_failure_without_payloads() -> Result<(), String> {
        let lines = RefCell::new(Vec::new());
        let say = |line: &str| lines.borrow_mut().push(line.to_owned());
        assert_eq!(timed("api1", "start", &say, async { Ok(7) }).await?, 7);
        let secret = "dummy-private-key-not-for-logging";
        let result: Result<(), String> = timed("api1", "bootstrap", &say, async { Err(secret.to_owned()) }).await;
        assert_eq!(result, Err(secret.to_owned()));
        let log = lines.into_inner().join("\n");
        assert!(log.contains("api1: start started"));
        assert!(log.contains("api1: start completed in ") && log.contains(" ms"));
        assert!(log.contains("api1: bootstrap failed in "));
        assert!(!log.contains(secret));
        Ok(())
    }

    #[tokio::test]
    async fn health_attempts_report_failure_retry_success_and_elapsed_time() -> Result<(), Error> {
        let (client, server) = health_client(vec![
            (
                503,
                r#"{"error":{"code":"warming_up","message":"dummy-private-key-not-for-logging"}}"#,
            ),
            (200, r#"{"status":"ok"}"#),
        ])?;
        let lines = RefCell::new(Vec::new());
        let say = |line: &str| lines.borrow_mut().push(line.to_owned());
        wait_health(&client, "api1", "after start", Duration::from_secs(10), &say).await?;
        server.join().map_err(|_| "health fixture panicked")??;
        let log = lines.into_inner().join("\n");
        assert!(log.contains("api1: HTTPS health after start attempt 1 started"));
        assert!(log.contains("attempt 1 failed after "));
        assert!(log.contains("HTTP 503, code=warming_up"));
        assert!(log.contains("retrying in 2000 ms"));
        assert!(log.contains("attempt 2 succeeded after "));
        assert!(log.contains("(elapsed "));
        assert!(!log.contains("dummy-private-key-not-for-logging"));
        assert!(!log.contains("dummy-client-token-not-for-logging"));
        Ok(())
    }

    #[tokio::test]
    async fn health_deadline_failure_is_logged_without_another_attempt() -> Result<(), Error> {
        let (client, server) = health_client(vec![(503, "null")])?;
        let lines = RefCell::new(Vec::new());
        let say = |line: &str| lines.borrow_mut().push(line.to_owned());
        let result = wait_health(&client, "api3", "after reboot", Duration::ZERO, &say).await;
        server.join().map_err(|_| "health fixture panicked")??;
        assert!(result.is_err());
        let log = lines.into_inner().join("\n");
        assert!(log.contains("api3: HTTPS health after reboot attempt 1 failed after "));
        assert!(!log.contains("attempt 2") && !log.contains("retrying"));
        Ok(())
    }

    #[tokio::test]
    async fn configure_verifies_reported_version_immediately_on_instance() -> Result<(), Error> {
        let instance = InstanceSpec {
            name: "api1".to_owned(),
            node: "pve1".to_owned(),
            ctid: 99_951,
            address: "127.0.0.1".parse()?,
            storage: "local-lvm".to_owned(),
        };
        let (client, server) = health_client(vec![
            (200, r#"{"status":"ok"}"#),
            (200, r#"{"config_version":null}"#),
            (200, r#"{"config_version":"ee82a4dfb6bc0011"}"#),
            (200, r#"{"config_version":"ee82a4dfb6bc0011"}"#),
        ])?;
        let lines = RefCell::new(Vec::new());
        let say = |line: &str| lines.borrow_mut().push(line.to_owned());
        configure(&client, &instance, "root@pam!api=secret", "domain = 'x'\n", &say).await?;
        server.join().map_err(|_| "configure fixture panicked")??;
        let log = lines.into_inner().join("\n");
        assert!(log.contains("api1: verify configuration version completed"));
        assert!(log.contains("ready, configuration ee82a4dfb6bc"));

        let (mismatch_client, mismatch_server) = health_client(vec![
            (200, r#"{"status":"ok"}"#),
            (200, r#"{"config_version":null}"#),
            (200, r#"{"config_version":"ee82a4dfb6bc0011"}"#),
            (200, r#"{"config_version":"0000000000000000"}"#),
        ])?;
        let Err(err) = configure(
            &mismatch_client,
            &instance,
            "root@pam!api=secret",
            "domain = 'x'\n",
            &|_| {},
        )
        .await
        else {
            return Err("mismatched reported version must fail immediately".into());
        };
        mismatch_server.join().map_err(|_| "mismatch fixture panicked")??;
        assert!(
            err.contains("reported configuration") && err.contains("does not match"),
            "{err}"
        );
        Ok(())
    }
}
