//! Critical lifecycle safety checks; simulated behavior is not live E2E evidence.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::lifecycle::{LifecycleTiming, destroy_workspace, plan_destroy};
use hs_core::tasks::NoObserver;
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::{ClusterSpec, PveSim};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const NODE: &str = "pve-example-1";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";

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

const CLUSTER: &str = include_str!("data/lifecycle-cluster.toml");

fn config() -> Result<Config, Error> {
    Ok(Config::parse(SERVER_TOML)?)
}

fn client(sim: &PveSim, token: &str) -> Result<PveClient, Error> {
    let mut client = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(token)?);
    client.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(client)?)
}

async fn cluster(faults: &str) -> Result<(PveSim, PveClient), Error> {
    let spec = ClusterSpec::from_scenario(&format!("{CLUSTER}\n{faults}"))?;
    let sim = PveSim::start(spec).await?;
    let pve = client(&sim, TOKEN)?;
    Ok((sim, pve))
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

#[tokio::test]
async fn destroy_refuses_a_workspace_outside_the_pool() -> TestResult {
    let (sim, pve) = cluster("").await?;
    let cfg = config()?;
    assert!(matches!(
        plan_destroy(&pve, &cfg, POOL, 150).await,
        Err(CoreError::OutsidePool(message)) if message.contains("not in pool homestack")
    ));
    let forged = serde_json::json!({"command": "destroy", "vmid": 150, "node": NODE, "name": "old-ws", "home_label": "HS_HOME_150", "ip": "192.0.2.150"});
    assert!(matches!(
        destroy_workspace(&pve, &cfg, POOL, &forged, &quick(), &NoObserver).await,
        Err(CoreError::OutsidePool(_))
    ));
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert!(ops.iter().all(|op| op.starts_with("get{")), "{ops:?}");
    Ok(())
}

#[tokio::test]
async fn destroy_execution_rejects_alien_before_shutdown() -> TestResult {
    let (sim, pve) = cluster("").await?;
    let cfg = config()?;
    let plan = serde_json::json!({"vmid": 160, "node": NODE, "name": "alien-workspace", "home_label": "HS_HOME_160", "ip": "192.0.2.160"});
    match destroy_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await {
        Err(CoreError::Refused(message)) => assert!(message.contains("forbidden workspace role tag"), "{message}"),
        other => return Err(format!("expected a refusal, got {other:?}").into()),
    }
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert!(ops.iter().all(|op| !op.contains("status/shutdown")), "{ops:?}");
    Ok(())
}
