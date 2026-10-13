//! The gate of phase 6 on the simulator, with the server as its own
//! process: `create` → a file in the home → `refresh` → the file is there
//! and the root is new → a `refresh` whose server is killed (`SIGKILL`)
//! in the middle → a new server process plans the recovery and carries it
//! out → `destroy`. `live_refresh.rs` runs the same on the cluster.

mod process;

use std::time::Duration;

use hs_pve::{PveClient, PveClientConfig, PveToken};
use process::{Error, Launch, ServerProcess};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use serde_json::{Value, json};

const CLUSTER: &str = include_str!("../../hs-core/tests/data/lifecycle-cluster.toml");
const PVE_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const NODE: &str = "pve-example-1";
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
const LIMIT: Duration = Duration::from_secs(60);

fn pve(sim: &PveSim) -> Result<PveClient, Error> {
    let mut config = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    config.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(config)?)
}

async fn vm_config(pve: &PveClient) -> Result<Value, Error> {
    Ok(pve.get_value(&["nodes", NODE, "qemu", "240", "config"], &[]).await?)
}

async fn guest_file(pve: &PveClient, file: &str) -> Option<String> {
    pve.get_value(&["nodes", NODE, "qemu", "240", "agent", "file-read"], &[("file", file)])
        .await
        .ok()
        .and_then(|read| read["content"].as_str().map(str::to_owned))
}

fn root(config: &Value) -> String {
    config["scsi0"]
        .as_str()
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn a_killed_refresh_is_recovered_by_the_next_server() -> Result<(), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = pve(&sim)?;
    let dir = tempfile::tempdir()?;
    let token = dir.path().join("pve-token");
    std::fs::write(&token, PVE_TOKEN)?;
    let launch = Launch {
        endpoints: sim.base_url(),
        pve_token_file: token,
        ca_pem: Some(sim.ca_pem().to_owned()),
        pool: "homestack".to_owned(),
        gold_pool: "homestack-gold".to_owned(),
    };
    let server = ServerProcess::start(&launch, dir.path()).await?;
    let version = server.configure(SERVER_TOML).await?;

    let job = server
        .run(
            "workspace.create",
            json!({"vmid": 240, "name": "hst-refresh"}),
            &version,
            LIMIT,
        )
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    for (file, content) in [("/home/developer/marker", "kept"), ("/root/marker", "old root")] {
        pve.post_value(
            &["nodes", NODE, "qemu", "240", "agent", "file-write"],
            &[("file", file), ("content", content)],
        )
        .await?;
    }
    let old_root = root(&vm_config(&pve).await?);

    let job = server
        .run("workspace.refresh", json!({"vmid": 240}), &version, LIMIT)
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    let refreshed = vm_config(&pve).await?;
    let new_root = root(&refreshed);
    assert_ne!(new_root, old_root, "the root is new");
    assert_eq!(
        guest_file(&pve, "/home/developer/marker").await.as_deref(),
        Some("kept")
    );
    assert_eq!(guest_file(&pve, "/root/marker").await, None);

    // Kill the server while the new root is attached (phase switched): the
    // Cloud-Init call (the third config change of the refresh) never returns.
    let (_, plan) = server.plan("workspace.refresh", json!({"vmid": 240})).await?;
    sim.inject_fault(FaultSpec {
        op: format!("post{{path=/nodes/{NODE}/qemu/240/config}}"),
        occurrence: 3,
        status: 500,
        message: String::new(),
        applied: false,
        hang: true,
    })?;
    let hangs = sim.hangs();
    server.apply(&plan, &version).await?;
    let deadline = tokio::time::Instant::now() + LIMIT;
    while sim.hangs() == hangs {
        if tokio::time::Instant::now() >= deadline {
            return Err("the refresh never reached the Cloud-Init step".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    server.kill()?;
    let interrupted = vm_config(&pve).await?;
    assert!(
        interrupted["description"]
            .as_str()
            .unwrap_or_default()
            .contains("homestack-journal"),
        "{interrupted}"
    );

    let server = ServerProcess::start(&launch, dir.path()).await?;
    let (status, plan) = server.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert!(status.is_success(), "{plan}");
    assert_eq!(plan["summary"]["mode"], "recover", "{plan}");
    assert_eq!(plan["summary"]["recovery_phase"], "switched", "{plan}");
    let job = server.apply(&plan, &version).await?;
    let job = server.finished(&job, LIMIT).await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["recovery"], "rolled back to original root", "{job}");
    let recovered = vm_config(&pve).await?;
    assert_eq!(root(&recovered), new_root, "back on the root before the killed refresh");
    assert!(recovered.get("description").is_none(), "{recovered}");
    assert!(
        !recovered
            .as_object()
            .into_iter()
            .flatten()
            .any(|(key, _)| key.starts_with("unused")),
        "{recovered}"
    );
    assert_eq!(
        guest_file(&pve, "/home/developer/marker").await.as_deref(),
        Some("kept")
    );

    let job = server
        .run("workspace.destroy", json!({"vmid": 240}), &version, LIMIT)
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    Ok(())
}
