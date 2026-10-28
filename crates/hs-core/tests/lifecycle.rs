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

/// Template fidelity also protects the positively owned failed-clone cleanup
/// boundary: ordinary clones must not be misclassified as source templates.
#[tokio::test]
async fn template_clones_are_ordinary_and_failed_create_cleanup_keeps_the_source() -> TestResult {
    use hs_core::lifecycle::{CreateRequest, create_workspace, plan_create};
    use pve_sim::scenario::{FaultSpec, VmState};
    use serde_json::json;

    for template in [false, true] {
        let mut spec = ClusterSpec::from_scenario(CLUSTER)?;
        let gold = spec.vms.iter_mut().find(|vm| vm.vmid == 101).ok_or("missing Gold")?;
        gold.status = VmState::Stopped;
        gold.config
            .insert("memory".into(), serde_json::from_value(json!(2048))?);
        if template {
            gold.config.insert("template".into(), serde_json::from_value(json!(1))?);
        }
        let sim = PveSim::start(spec).await?;
        let pve = client(&sim, TOKEN)?;
        let cloned = pve
            .post_value(
                &["nodes", NODE, "qemu", "101", "clone"],
                &[
                    ("newid", "241"),
                    ("name", "clone"),
                    ("full", "1"),
                    ("pool", POOL),
                    ("storage", "local-lvm"),
                ],
            )
            .await?;
        assert!(cloned.is_string());
        let source = pve.get_value(&["nodes", NODE, "qemu", "101", "config"], &[]).await?;
        let clone = pve.get_value(&["nodes", NODE, "qemu", "241", "config"], &[]).await?;
        assert_eq!(source.get("template").is_some(), template);
        assert!(clone.get("template").is_none());
        assert_eq!(clone["memory"], json!(2048));
        assert_eq!(clone["agent"], source["agent"]);
        assert_eq!(clone["tags"], source["tags"]);
        assert_eq!(clone["scsi0"], json!("local-lvm:vm-241-disk-0,iothread=1,size=16G"));
        pve.post_value(&["nodes", NODE, "qemu", "241", "status", "start"], &[])
            .await?;
        let state = pve
            .get_value(&["nodes", NODE, "qemu", "241", "status", "current"], &[])
            .await?;
        assert_eq!(state["status"], "running");
        if template {
            assert!(
                pve.post_value(&["nodes", NODE, "qemu", "101", "status", "start"], &[])
                    .await
                    .is_err()
            );
            let state = pve
                .get_value(&["nodes", NODE, "qemu", "101", "status", "current"], &[])
                .await?;
            assert_eq!(state["status"], "stopped");
        }
        let request = CreateRequest {
            gold_vmid: None,
            vmid: 240,
            name: "cleanup-check".into(),
            home_size: None,
            storage: None,
            node: None,
        };
        let plan = plan_create(&pve, &config()?, POOL, &request).await?;
        sim.inject_fault(FaultSpec {
            op: format!("post{{path=/nodes/{NODE}/storage/local-lvm/content}}"),
            occurrence: 1,
            status: 500,
            message: "synthetic allocation failure".into(),
            applied: false,
            hang: false,
            task: None,
        })?;
        let result = create_workspace(&pve, &config()?, &plan, &quick(), &NoObserver).await;
        assert!(result.is_err());
        let resources = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
        let resources = resources.as_array().ok_or("missing resources")?;
        assert!(
            resources.iter().all(|vm| vm["vmid"] != 240),
            "failed owned clone was not removed: {result:?}"
        );
        assert!(resources.iter().any(|vm| vm["vmid"] == 101));
        let source = pve.get_value(&["nodes", NODE, "qemu", "101", "config"], &[]).await?;
        assert_eq!(source.get("template").is_some(), template);
        sim.shutdown().await;
    }
    Ok(())
}
