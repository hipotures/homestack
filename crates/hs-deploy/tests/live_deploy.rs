//! L4, the gate of phase 11 (`docs/migration/phase-11.md`): three server
//! instances, one per node, deployed through the Proxmox API only; their
//! `GET /v1/config/version` agree; with one of them stopped, a client of
//! all three still reads. The containers are removed at the end, whatever
//! happened.
//!
//! `scripts/check.sh --live` runs it with `HOMESTACK_LIVE_IMAGE` (the node
//! image CI published) and the files of `data/live-deploy.toml`: the deploy
//! token, the test token, the client token, the certificate from
//! `homestack-deploy cert`. It refuses anything but test instances: CTIDs
//! 997–999, pool `homestack-test`, the test token.

use std::path::Path;
use std::time::Duration;

use hs_client::{Client, ClientConfig, Endpoint};
use hs_deploy::config::DeployConfig;
use hs_deploy::{deploy, node};
use hs_pve::PveToken;

type Error = Box<dyn std::error::Error>;

fn load() -> Result<DeployConfig, Error> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/live-deploy.toml");
    let mut config = DeployConfig::load(&path)?;
    config.image = std::env::var("HOMESTACK_LIVE_IMAGE").map_err(|_| "HOMESTACK_LIVE_IMAGE is not set")?;
    if config.server.pool != "homestack-test" {
        return Err("the live gate deploys only test instances (pool homestack-test)".into());
    }
    if !PveToken::from_file(&config.server.pve_token_file)?
        .id()
        .ends_with("!test")
    {
        return Err("the instances' token is not the test token; refusing".into());
    }
    if config
        .instances
        .iter()
        .any(|instance| !(997..=999).contains(&instance.ctid))
    {
        return Err("the live gate uses CTIDs 997-999 only".into());
    }
    // The installed servers (homestack install writes deploy.toml) must keep
    // their containers, addresses and names: the gate stops before it starts.
    let installed = std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".config/homestack/deploy.toml"))
        .filter(|path| path.exists());
    if let Some(path) = installed {
        let clashes = config.clashes(&DeployConfig::load(&path)?);
        if !clashes.is_empty() {
            return Err(format!(
                "the live gate's test instances share {} with the installed servers of {}; refusing",
                clashes.join(", "),
                path.display()
            )
            .into());
        }
    }
    Ok(config)
}

#[tokio::test]
#[ignore = "live cluster, creates containers 997-999: run through scripts/check.sh --live"]
async fn three_instances_agree_and_survive_one_stopped() -> Result<(), Error> {
    let config = load()?;
    let say = |line: &str| eprintln!("live deploy: {line}");
    let outcome = gate(&config, &say).await;
    if let Err(error) = deploy::remove(&config, &[], &say).await {
        eprintln!("live deploy: removing the containers failed: {error}");
        return Err(error.into());
    }
    let pve = deploy::deploy_client(&config)?;
    for instance in &config.instances {
        if node::container(&pve, instance.ctid).await?.is_some() {
            return Err(format!("CT {} is left", instance.ctid).into());
        }
    }
    outcome
}

async fn gate(config: &DeployConfig, say: &impl Fn(&str)) -> Result<(), Error> {
    deploy::apply(config, &[], say).await?;

    // A client of the three by their names in DNS, as the desktop has it.
    let token = std::fs::read_to_string(&config.server.api_token_file)?
        .trim()
        .to_owned();
    let endpoints = config
        .instances
        .iter()
        .map(|instance| Ok(Endpoint::from(config.url(instance)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let mut client_config = ClientConfig::new(endpoints, token);
    client_config.connect_timeout = Duration::from_secs(3);
    let client = Client::new(client_config)?;
    let versions: Vec<_> = client.config_versions().await;
    let first = versions
        .first()
        .and_then(|(_, version)| version.as_ref().ok().cloned().flatten())
        .ok_or("no version from the first instance")?;
    for (url, version) in &versions {
        match version {
            Ok(Some(version)) if *version == first => say(&format!("{url}: configuration {}", &version[..12])),
            other => return Err(format!("{url}: {other:?}, expected {first}").into()),
        }
    }
    let status = client.get("/v1/status").await?;
    say(&format!(
        "status: Gold {}, {} workspaces",
        status["gold"]["vmid"],
        status["workspaces"].as_array().map_or(0, Vec::len)
    ));

    // Stop the first instance's container: reads go on through the others.
    let pve = deploy::deploy_client(config)?;
    let stopped = &config.instances[0];
    node::power(&pve, &stopped.node, stopped.ctid, "stop").await?;
    say(&format!("{} stopped", stopped.name));
    if client.get_on(0, "/health/live").await.is_ok() {
        return Err(format!("{} still answers after its container stopped", stopped.name).into());
    }
    for path in ["/v1/status", "/v1/storage", "/v1/config/version"] {
        let started = tokio::time::Instant::now();
        client
            .get(path)
            .await
            .map_err(|e| format!("{path} with {} stopped: {e}", stopped.name))?;
        say(&format!(
            "{path} answered with {} stopped ({} ms)",
            stopped.name,
            started.elapsed().as_millis()
        ));
    }
    // A plan goes to another instance too (VM 249 does not exist: refused
    // by an instance, not unanswered).
    match client.plan("workspace.destroy", serde_json::json!({"vmid": 249})).await {
        Ok(session) => say(&format!("plan made by {}", session.url())),
        Err(hs_client::ClientError::Api { url, code, .. }) => say(&format!("plan answered by {url}: {code}")),
        Err(error) => return Err(format!("no instance planned with {} stopped: {error}", stopped.name).into()),
    }
    Ok(())
}
