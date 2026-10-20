//! Critical resize safety checks; simulated behavior is not live E2E evidence.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::lifecycle::LifecycleTiming;
use hs_core::resize::{ResizeRequest, plan_resize, resize_workspace};
use hs_core::tasks::NoObserver;
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::{ClusterSpec, PveSim};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error>;
type TestResult = Result<(), Error>;

const NODE: &str = "pve-example-1";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const ADMIN: &str = "root@pam!sim=00000000-0000-0000-0000-00000000000a";
const CLUSTER: &str = include_str!("data/resize-cluster.toml");
const SERVER_TOML: &str = r#"version = 1
[node]
name = "pve-example-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["local-lvm"]
[root]
disk = "scsi0"
[home]
disk = "scsi1"
default_size = "20G"
[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.53"]
[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"
[user]
name = "developer"
uid = 1000
gid = 1000
"#;

const GIB: u64 = 1024 * 1024 * 1024;

fn config() -> Result<Config, Error> {
    Ok(Config::parse(SERVER_TOML)?)
}

fn client(sim: &PveSim, token: &str) -> Result<PveClient, Error> {
    let mut client = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(token)?);
    client.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(client)?)
}

fn quick() -> LifecycleTiming {
    LifecycleTiming {
        poll: Duration::from_millis(5),
        verify: VerifyTimeouts {
            agent: Duration::from_secs(5),
            cloud_init: Duration::from_secs(5),
        },
    }
}

async fn cluster() -> Result<(PveSim, PveClient), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = client(&sim, TOKEN)?;
    Ok((sim, pve))
}

fn root(vmid: u32, size: &str) -> ResizeRequest {
    ResizeRequest {
        vmid,
        root_size: Some(size.to_owned()),
        home_size: None,
    }
}

fn home(vmid: u32, size: &str) -> ResizeRequest {
    ResizeRequest {
        vmid,
        root_size: None,
        home_size: Some(size.to_owned()),
    }
}

fn ops(sim: &PveSim) -> Vec<String> {
    sim.ops().iter().map(ToString::to_string).collect()
}

fn guest_commands(sim: &PveSim) -> Vec<String> {
    sim.ops()
        .into_iter()
        .filter(|op| op.name == "guest_exec")
        .filter_map(|op| op.params.get("cmd").cloned())
        .collect()
}

fn resize_calls(sim: &PveSim) -> Vec<String> {
    ops(sim).into_iter().filter(|op| op.ends_with("/resize}")).collect()
}

async fn plan(pve: &PveClient, request: &ResizeRequest) -> Result<Value, CoreError> {
    plan_resize(
        pve,
        &config().map_err(|e| CoreError::Invalid(e.to_string()))?,
        POOL,
        request,
    )
    .await
}

async fn run(pve: &PveClient, plan: &Value, passphrase: Option<&str>) -> Result<Value, CoreError> {
    resize_workspace(pve, plan, passphrase, &quick(), &NoObserver).await
}

async fn filesystem_size(pve: &PveClient, vmid: u32, mount: &str) -> Result<u64, Error> {
    let vmid = vmid.to_string();
    let info = pve
        .get_value(&["nodes", NODE, "qemu", &vmid, "agent", "get-fsinfo"], &[])
        .await?;
    info["result"]
        .as_array()
        .and_then(|list| list.iter().find(|fs| fs["mountpoint"] == mount))
        .and_then(|fs| fs["total-bytes-privileged"].as_u64())
        .ok_or_else(|| format!("no filesystem at {mount}: {info}").into())
}

async fn disk_config(pve: &PveClient, vmid: u32, slot: &str) -> Result<String, Error> {
    let vmid = vmid.to_string();
    let config = pve.get_value(&["nodes", NODE, "qemu", &vmid, "config"], &[]).await?;
    Ok(config[slot].as_str().unwrap_or_default().to_owned())
}

fn message(result: Result<Value, CoreError>) -> String {
    match result {
        Ok(value) => panic!("expected a failure, got {value}"),
        Err(error) => error.to_string(),
    }
}

#[tokio::test]
async fn shrink_rejected_before_mutations_and_guest_inspection() -> TestResult {
    let (sim, pve) = cluster().await?;
    let error = plan(&pve, &root(200, "8G"))
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert_eq!(error, "Shrinking is not supported: scsi0 is 16G, requested 8G");
    assert!(guest_commands(&sim).is_empty());
    assert!(resize_calls(&sim).is_empty());
    Ok(())
}

#[tokio::test]
async fn home_selects_home_disk_and_skips_partition_growth() -> TestResult {
    let (sim, pve) = cluster().await?;
    let plan = plan(&pve, &home(200, "1T")).await?;
    assert_eq!(plan["disk"], "scsi1");
    assert_eq!(plan["guest"]["device"], "/dev/sdb");
    sim.clear_ops();
    run(&pve, &plan, None).await?;
    assert_eq!(resize_calls(&sim).len(), 1);
    assert!(disk_config(&pve, 200, "scsi1").await?.ends_with("size=1T"));
    let commands = guest_commands(&sim);
    assert!(!commands.iter().any(|c| c.starts_with("growpart")), "{commands:?}");
    assert!(commands.contains(&"resize2fs /dev/sdb".to_owned()), "{commands:?}");
    assert_eq!(filesystem_size(&pve, 200, "/home/developer").await?, 1024 * GIB);
    Ok(())
}

#[tokio::test]
async fn revalidation_rejects_volume_or_size_drift() -> TestResult {
    let (sim, pve) = cluster().await?;
    let planned = plan(&pve, &root(200, "32G")).await?;
    let mut other_volume = planned.clone();
    other_volume["volume"] = json!("local-lvm:vm-200-disk-9");
    sim.clear_ops();
    let error = message(run(&pve, &other_volume, None).await);
    assert!(error.contains("changed after resize confirmation"), "{error}");
    assert!(resize_calls(&sim).is_empty());

    let admin = client(&sim, ADMIN)?;
    admin
        .put_value(
            &["nodes", NODE, "qemu", "200", "resize"],
            &[("disk", "scsi0"), ("size", "64G")],
        )
        .await?;
    sim.clear_ops();
    let error = message(run(&pve, &planned, None).await);
    assert!(error.contains("changed after resize confirmation"), "{error}");
    assert!(resize_calls(&sim).is_empty());
    Ok(())
}
