//! `create` and `destroy` (B1, B7) on the simulator, and the Python tests of
//! phase 5 (`docs/migration/phase-5.md`).
//!
//! The cluster below is what `deploy/pve/homestack-pve.sh install` makes:
//! Gold in `homestack-gold`, an empty `homestack` pool, and a token with the
//! script's roles. VM 150 is a workspace outside the pool (made before it),
//! VM 160 an alien VM someone put in the pool.

use std::time::Duration;

use hs_core::CoreError;
use hs_core::gold::boot_order_contains_disk;
use hs_core::inventory::{disk_option, disk_storage, home_label, parse_disk_size_gb};
use hs_core::lifecycle::{
    CreateRequest, LifecycleTiming, create_workspace, destroy_workspace, home_volume_name, parse_home_size,
    plan_create, plan_destroy, validate_name,
};
use hs_core::tasks::NoObserver;
use hs_core::verify::VerifyTimeouts;
use hs_model::config::Config;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::{ClusterSpec, PveSim};
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const NODE: &str = "pve-example-1";
const POOL: &str = "homestack";
const TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
/// Sees everything, for checking what is left.
const ADMIN: &str = "root@pam!sim=00000000-0000-0000-0000-00000000000a";

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

fn request(vmid: u32, name: &str) -> CreateRequest {
    CreateRequest {
        gold_vmid: None,
        vmid,
        name: name.to_owned(),
        home_size: None,
        storage: None,
        node: None,
    }
}

/// What the simulator holds for VMID `vmid`: the VM, its volumes, its pool.
async fn traces(sim: &PveSim, vmid: u32) -> Result<Vec<String>, Error> {
    let pve = client(sim, ADMIN)?;
    let mut found = Vec::new();
    let resources = pve.get_value(&["cluster", "resources"], &[("type", "vm")]).await?;
    if resources
        .as_array()
        .into_iter()
        .flatten()
        .any(|vm| vm.get("vmid").and_then(Value::as_u64) == Some(u64::from(vmid)))
    {
        found.push(format!("VM {vmid}"));
    }
    let volumes = pve
        .get_value(&["nodes", NODE, "storage", "local-lvm", "content"], &[])
        .await?;
    for volume in volumes.as_array().into_iter().flatten() {
        let volid = volume.get("volid").and_then(Value::as_str).unwrap_or_default();
        if volid.contains(&format!("vm-{vmid}-")) {
            found.push(volid.to_owned());
        }
    }
    let pool = pve.get_value(&["pools"], &[("poolid", POOL)]).await?;
    if pool.to_string().contains(&format!("\"vmid\":{vmid}")) {
        found.push(format!("pool member {vmid}"));
    }
    Ok(found)
}

// --- ports of the Python tests ---------------------------------------------

/// `WorkspaceTargetTests::test_create_name_cannot_start_with_digit`
#[test]
fn create_name_cannot_start_with_digit() {
    assert!(validate_name("test1").is_ok());
    assert!(matches!(validate_name("200test"), Err(CoreError::Invalid(_))));
    assert!(validate_name(&format!("a{}", "b".repeat(62))).is_ok());
    assert!(validate_name(&format!("a{}", "b".repeat(63))).is_err());
    assert!(validate_name("a_b").is_err());
}

/// `HomeDiskTests::test_home_label_and_size`
#[test]
fn home_label_and_size() -> TestResult {
    assert_eq!(home_label(200)?, "HS_HOME_200");
    assert_eq!(parse_home_size("20G")?, ("20G".to_owned(), 20 * 1024));
    assert_eq!(parse_home_size("1t")?, ("1T".to_owned(), 1024 * 1024));
    assert_eq!(parse_home_size("500m")?, ("500M".to_owned(), 500));
    assert!(parse_home_size("0M").is_err());
    assert!(parse_home_size("1.5G").is_err());
    assert!(parse_home_size("0G").is_err());
    Ok(())
}

/// `HomeDiskTests::test_logical_volume_names`, the home part: the root
/// keeps Proxmox's name (decision 2 = A).
#[test]
fn logical_volume_names() -> TestResult {
    assert_eq!(home_volume_name(200, "user")?, "vm-200-hs-home-user");
    assert!(home_volume_name(200, "1user").is_err());
    Ok(())
}

/// `HomeDiskTests::test_disk_option`
#[test]
fn disk_option_reads_disk_settings() {
    let disk = "example-storage-a:vm-200-disk-1,discard=on,iothread=1,size=20G,serial=HS_HOME_200,ssd=1";
    assert_eq!(disk_option(disk, "serial").as_deref(), Some("HS_HOME_200"));
    assert_eq!(disk_storage(Some(disk)).as_deref(), Some("example-storage-a"));
    assert_eq!(parse_disk_size_gb(disk), Some(20.0));
}

/// `HomeDiskTests::test_boot_order_contains_root_disk`
#[test]
fn boot_order_contains_root_disk() {
    assert!(boot_order_contains_disk("order=ide2;scsi0;net0", "scsi0"));
    assert!(!boot_order_contains_disk("order=ide2;net0", "scsi0"));
    assert!(!boot_order_contains_disk("", "scsi0"));
}

/// `CreateExecutionTests::test_same_node_default_plan_uses_clone_only`
#[tokio::test]
async fn same_node_default_plan_uses_clone_only() -> TestResult {
    let (sim, pve) = cluster("").await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(200, "test1")).await?;
    assert_eq!(plan["node"], NODE);
    assert_eq!(plan["source_node"], NODE);
    assert_eq!(plan["transfer_method"], "clone");
    assert_eq!(plan["pool"], POOL);
    assert_eq!(plan["ip"], "192.0.2.200");
    assert_eq!(plan["home_size"], "20G");
    assert_eq!(plan["home_size_mib"], 20 * 1024);
    assert_eq!(plan["home_label"], "HS_HOME_200");
    assert_eq!(plan["root_storage"], "local-lvm");
    assert_eq!(plan["ssh_public_keys"], serde_json::json!(["developer@desktop"]));
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert!(
        ops.iter().all(|op| op.starts_with("get{")),
        "planning changes nothing: {ops:?}"
    );

    // Another node is B2 (phase 10, `create_transfer.rs`); this cluster has
    // only one.
    let mut elsewhere = request(200, "test1");
    elsewhere.node = Some("pve-example-2".to_owned());
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &elsewhere).await,
        Err(CoreError::Refused(message)) if message == "Target node 'pve-example-2' is not in the Proxmox cluster"
    ));
    Ok(())
}

/// `CreateWorkspaceCompletionTests::test_create_result_has_no_sync_command`,
/// and the rest of a create on the simulator: clone into the pool, a named
/// home with the serial, the role tag, native Cloud-Init, start and the
/// guest verification.
#[tokio::test]
async fn create_clones_into_the_pool_and_verifies_the_guest() -> TestResult {
    let (sim, pve) = cluster("").await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(200, "test1")).await?;
    let result = create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await?;

    assert!(result.get("sync_command").is_none());
    assert_eq!(result["event"], "workspace.created");
    assert_eq!(result["vmid"], 200);
    assert_eq!(result["ip"], "192.0.2.200");
    assert_eq!(result["status"], "running");
    assert_eq!(result["mac"], "BC:24:11:00:01:01");
    assert_eq!(result["cloud_id"], "nocloud");
    assert_eq!(result["host_key_changed"], true);
    assert_eq!(
        result["ssh_public_keys"],
        serde_json::json!([{"label": "developer@desktop", "verified": true}])
    );

    let config = pve.get_value(&["nodes", NODE, "qemu", "200", "config"], &[]).await?;
    assert_eq!(config["tags"], "homestack-ws");
    assert_eq!(
        config["scsi1"],
        "local-lvm:vm-200-hs-home-developer,discard=on,iothread=1,ssd=1,serial=HS_HOME_200"
    );
    assert_eq!(config["scsi0"], "local-lvm:vm-200-disk-0,iothread=1,size=16G");
    assert_eq!(config["cicustom"], "vendor=local:snippets/homestack-vendor-create.yaml");
    assert_eq!(config["ipconfig0"], "ip=192.0.2.200/24,gw=192.0.2.1");
    assert_eq!(config["ciuser"], "root");
    assert_eq!(
        traces(&sim, 200).await?,
        [
            "VM 200",
            "local-lvm:vm-200-cloudinit",
            "local-lvm:vm-200-disk-0",
            "local-lvm:vm-200-hs-home-developer",
            "pool member 200"
        ]
    );

    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert!(
        ops.iter().all(|op| !op.starts_with("guest_exec")),
        "no exec in the workspace (decision 5): {ops:?}"
    );
    Ok(())
}

/// The clone keeps Gold's `net0`, so the token needs `SDN.Use` on its
/// bridge; the plan says so before anything is made.
#[tokio::test]
async fn create_needs_sdn_use_on_golds_bridge() -> TestResult {
    let (sim, _pve) = cluster("").await?;
    let pve = client(&sim, "homestack@pve!nosdn=00000000-0000-0000-0000-00000000000e")?;
    match plan_create(&pve, &config()?, POOL, &request(200, "test1")).await {
        Err(CoreError::Refused(message)) => {
            assert!(
                message.contains("SDN.Use on /sdn/zones/localnetwork/vmbr0"),
                "{message}"
            );
        }
        other => return Err(format!("expected a refusal, got {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn create_refuses_a_used_vmid_a_missing_pool_and_bad_input() -> TestResult {
    let (_sim, pve) = cluster("").await?;
    let cfg = config()?;
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &request(150, "dup")).await,
        Err(CoreError::Refused(message)) if message == "VMID 150 already exists"
    ));
    assert!(matches!(
        plan_create(&pve, &cfg, "nonexistent", &request(200, "test1")).await,
        Err(CoreError::Refused(message)) if message.contains("pool nonexistent does not exist")
    ));
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &request(255, "test1")).await,
        Err(CoreError::Invalid(_))
    ));
    let mut big = request(200, "test1");
    big.home_size = Some("500K".to_owned());
    assert!(matches!(
        plan_create(&pve, &cfg, POOL, &big).await,
        Err(CoreError::Invalid(_))
    ));
    let mut other = request(200, "test1");
    other.storage = Some("local".to_owned());
    assert!(plan_create(&pve, &cfg, POOL, &other).await.is_err());
    Ok(())
}

/// Python's cleanup rules after a failure in each step of create: before
/// the start the VM is destroyed (with the home it holds, and a home that
/// could not be attached is freed); nothing of the VMID is left. After the
/// start nothing is removed.
#[tokio::test]
async fn a_failure_before_the_start_leaves_nothing() -> TestResult {
    let steps = [
        ("clone", "post{path=/nodes/pve-example-1/qemu/101/clone}", 1),
        (
            "home allocation",
            "post{path=/nodes/pve-example-1/storage/local-lvm/content}",
            1,
        ),
        ("home attachment", "post{path=/nodes/pve-example-1/qemu/200/config}", 1),
        ("role tag", "post{path=/nodes/pve-example-1/qemu/200/config}", 2),
        ("cloud-init", "post{path=/nodes/pve-example-1/qemu/200/config}", 3),
        ("start", "post{path=/nodes/pve-example-1/qemu/200/status/start}", 1),
    ];
    for (step, op, occurrence) in steps {
        let fault = format!(
            "[[pve.faults]]\nop = \"{op}\"\noccurrence = {occurrence}\nstatus = 500\nmessage = \"injected {step} failure\"\n"
        );
        let (sim, pve) = cluster(&fault).await?;
        let cfg = config()?;
        let plan = plan_create(&pve, &cfg, POOL, &request(200, "test1")).await?;
        let error = match create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await {
            Ok(result) => return Err(format!("{step}: create succeeded: {result}").into()),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(&format!("injected {step} failure")), "{step}: {error}");
        assert!(!error.contains("Cleanup report"), "{step}: cleanup failed: {error}");
        assert_eq!(traces(&sim, 200).await?, Vec::<String>::new(), "{step}: {error}");
    }
    Ok(())
}

#[tokio::test]
async fn a_failure_after_the_start_leaves_the_vm() -> TestResult {
    let fault = "[[pve.faults]]\nop = \"get{path=/nodes/pve-example-1/qemu/200/agent/get-host-name}\"\n\
                 occurrence = 1\nstatus = 500\nmessage = \"agent read failed\"\n";
    let (sim, pve) = cluster(fault).await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(200, "test1")).await?;
    assert!(
        create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver)
            .await
            .is_err()
    );
    assert!(traces(&sim, 200).await?.contains(&"VM 200".to_owned()));
    Ok(())
}

/// Create, verify and destroy twice in a row: nothing is left (the gate of
/// phase 5, on the simulator).
#[tokio::test]
async fn create_and_destroy_twice_leave_nothing() -> TestResult {
    let (sim, pve) = cluster("").await?;
    let cfg = config()?;
    for round in 0..2 {
        let plan = plan_create(&pve, &cfg, POOL, &request(240, "hst-cycle")).await?;
        create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await?;
        let plan = plan_destroy(&pve, &cfg, POOL, 240).await?;
        assert_eq!(plan["home_label"], "HS_HOME_240");
        assert_eq!(plan["name"], "hst-cycle");
        let result = destroy_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
        assert_eq!(result["event"], "workspace.destroyed", "round {round}");
        assert_eq!(result["status"], "absent");
        assert_eq!(result["ip"], "192.0.2.240");
        assert_eq!(result["node_files_left"], serde_json::json!([]));
        assert_eq!(traces(&sim, 240).await?, Vec::<String>::new(), "round {round}");
    }
    Ok(())
}

/// The gate before phase 5: the server refuses a VM outside its pool even
/// when the tags are right, before any change.
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

/// `LifecycleAlienGuardsTests::test_destroy_execution_rejects_alien_before_shutdown`
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

/// A workspace Python made has its four snippets on the nodes; the API
/// cannot delete them, so destroy names them.
#[tokio::test]
async fn destroy_names_the_node_files_of_a_python_workspace() -> TestResult {
    let (_sim, pve) = cluster("").await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(201, "pyws")).await?;
    create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await?;
    let cicustom = "user=local:snippets/homestack-pyws-user.yaml,vendor=local:snippets/homestack-pyws-vendor.yaml,\
                    network=local:snippets/homestack-pyws-network.yaml,meta=local:snippets/homestack-pyws-meta.yaml";
    let upid = pve
        .post_value(&["nodes", NODE, "qemu", "201", "config"], &[("cicustom", cicustom)])
        .await?;
    assert!(upid.as_str().is_some());
    let plan = plan_destroy(&pve, &cfg, POOL, 201).await?;
    let result = destroy_workspace(&pve, &cfg, POOL, &plan, &quick(), &NoObserver).await?;
    assert_eq!(
        result["node_files_left"],
        serde_json::json!([
            "/var/lib/vz/snippets/homestack-pyws-user.yaml",
            "/var/lib/vz/snippets/homestack-pyws-vendor.yaml",
            "/var/lib/vz/snippets/homestack-pyws-network.yaml",
            "/var/lib/vz/snippets/homestack-pyws-meta.yaml",
            "/var/lib/vz/snippets/homestack-vm201-refresh.json",
        ])
    );
    Ok(())
}

#[test]
fn a_large_vmid_gets_the_address_of_its_last_two_octets() -> Result<(), Error> {
    use hs_core::lifecycle::derive_ip;
    let mut config = config()?;
    config.network.prefix = "192.168.100".to_owned();
    config.network.gateway = "192.168.100.1".to_owned();
    config.network.cidr = 22;
    assert_eq!(derive_ip(&config, 240)?, "192.168.100.240");
    assert_eq!(derive_ip(&config, 101_010)?, "192.168.101.10");
    let refused = derive_ip(&config, 104_010).map_err(|error| error.to_string());
    assert!(
        refused.is_err_and(|error| error.contains("192.168.100.0/22")),
        "outside the network"
    );
    config.network.cidr = 24;
    assert!(derive_ip(&config, 101_010).is_err(), "a /24 has no 192.168.101.x");
    Ok(())
}

/// Start and stop (owner, 2026-10-14; Python had neither): a workspace of
/// the pool is shut down cleanly and started again; one outside the pool is
/// refused before any change.
#[tokio::test]
async fn a_workspace_is_stopped_and_started_and_one_outside_the_pool_refused() -> TestResult {
    use hs_core::lifecycle::{plan_power, power_vm};
    let (sim, pve) = cluster("").await?;
    let cfg = config()?;
    let plan = plan_create(&pve, &cfg, POOL, &request(240, "hst-power")).await?;
    create_workspace(&pve, &cfg, &plan, &quick(), &NoObserver).await?;

    let plan = plan_power(&pve, &cfg, POOL, "homestack-gold", 240, false).await?;
    assert_eq!(
        (plan["command"].as_str(), plan["status"].as_str()),
        (Some("stop"), Some("running"))
    );
    let result = power_vm(&pve, &cfg, POOL, "homestack-gold", &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["event"], "workspace.stopped");
    assert_eq!(result["status"], "stopped");

    let plan = plan_power(&pve, &cfg, POOL, "homestack-gold", 240, true).await?;
    let result = power_vm(&pve, &cfg, POOL, "homestack-gold", &plan, &quick(), &NoObserver).await?;
    assert_eq!(result["event"], "workspace.started");
    assert_eq!(result["status"], "running");

    let before = sim.ops().len();
    assert!(matches!(
        plan_power(&pve, &cfg, POOL, "homestack-gold", 150, false).await,
        Err(CoreError::OutsidePool(_))
    ));
    assert!(
        sim.ops()[before..].iter().all(|op| op.to_string().starts_with("get{")),
        "nothing changed outside the pool"
    );
    Ok(())
}

#[tokio::test]
async fn configured_gold_has_manual_power_in_its_own_pool() -> TestResult {
    use hs_core::lifecycle::{plan_power, power_vm};
    let (_sim, pve) = cluster("").await?;
    let cfg = config()?;
    for (start, event, status) in [(false, "gold.stopped", "stopped"), (true, "gold.started", "running")] {
        let plan = plan_power(&pve, &cfg, POOL, "homestack-gold", 101, start).await?;
        assert_eq!(plan["role"], "GOLD");
        assert_eq!(plan["pool"], "homestack-gold");
        let result = power_vm(&pve, &cfg, POOL, "homestack-gold", &plan, &quick(), &NoObserver).await?;
        assert_eq!(result["event"], event);
        assert_eq!(result["status"], status);
    }
    assert!(plan_power(&pve, &cfg, POOL, "wrong-pool", 101, true).await.is_err());
    let mut other = cfg.clone();
    other.gold_vmid = Some(150);
    assert!(
        plan_power(&pve, &other, POOL, "homestack-gold", 101, true)
            .await
            .is_err()
    );
    let mut stale = plan_power(&pve, &cfg, POOL, "homestack-gold", 101, true).await?;
    stale["node"] = "different-node".into();
    assert!(
        power_vm(&pve, &cfg, POOL, "homestack-gold", &stale, &quick(), &NoObserver)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn gold_power_refuses_templates_and_wrong_or_locked_roles() -> TestResult {
    use hs_core::lifecycle::plan_power;
    for (key, value) in [
        ("tags", "homestack-ws"),
        ("tags", "homestack-gold;homestack-ws"),
        ("tags", "homestack-gold;homestack-lock"),
        ("template", "1"),
    ] {
        let mut spec = ClusterSpec::from_scenario(CLUSTER)?;
        let gold = spec
            .vms
            .iter_mut()
            .find(|vm| vm.vmid == 101)
            .ok_or("fixture has no Gold VM")?;
        gold.config.insert(key.into(), value.into());
        let sim = PveSim::start(spec).await?;
        let pve = client(&sim, TOKEN)?;
        assert!(
            plan_power(&pve, &config()?, POOL, "homestack-gold", 101, true)
                .await
                .is_err(),
            "{key}={value}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn create_capacity_includes_clone_and_home_before_any_allocation() -> TestResult {
    let (sim, pve) = cluster("").await?;
    let required = (16 + 20) * 1024_u64.pow(3) + 4 * 1024 * 1024;
    sim.set_storage_available(NODE, "local-lvm", required - 1)?;
    let error = plan_create(&pve, &config()?, POOL, &request(200, "test1"))
        .await
        .err()
        .ok_or("expected refusal")?;
    assert!(
        error.to_string().contains(&format!("requires {required} bytes")),
        "{error}"
    );
    assert!(sim.ops().iter().all(|op| op.name == "get" || op.name == "guest_ping"));
    assert!(traces(&sim, 200).await?.is_empty());
    sim.set_storage_available(NODE, "local-lvm", required)?;
    let plan = plan_create(&pve, &config()?, POOL, &request(200, "test1")).await?;
    assert_eq!(plan["capacity"]["target"]["required_bytes"], required);
    sim.set_storage_available(NODE, "local-lvm", 0)?;
    sim.clear_ops();
    let error = create_workspace(&pve, &config()?, &plan, &quick(), &NoObserver)
        .await
        .err()
        .ok_or("expected refusal")?;
    assert!(error.to_string().contains("available 0 bytes"), "{error}");
    assert!(sim.ops().iter().all(|op| op.name == "get" || op.name == "guest_ping"));
    assert!(traces(&sim, 200).await?.is_empty());
    Ok(())
}
