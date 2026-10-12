//! VMs, storages, networks and the guest agent of the simulated cluster.
//! Error texts and response shapes were recorded on PVE 9.2.21; privileges
//! follow the PVE API documentation (`apidoc.js`) and, for volume listing,
//! `PVE::Storage::check_volume_access`.

use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const ROOT: &str = "root@pam!t=00000000-0000-0000-0000-000000000001";
const AUDITOR: &str = "homestack@pve!server=00000000-0000-0000-0000-000000000002";
const DISK_READER: &str = "reader@pve!t=00000000-0000-0000-0000-000000000003";

fn spec() -> Result<ClusterSpec, toml::de::Error> {
    toml::from_str(
        r#"
        version = { release = "9.2", version = "9.2.21", repoid = "4f6e0ac86f9e8c7f" }

        [[nodes]]
        name = "pve1"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve"
        uptime = 1000
        dns = { dns1 = "192.0.2.53", search = "example" }
        [[nodes.storages]]
        storage = "fast"
        total = 1000
        used = 250
        avail = 750
        [[nodes.network]]
        iface = "vmbr0"
        type = "bridge"
        cidr = "192.0.2.11/24"
        gateway = "192.0.2.1"

        [[nodes]]
        name = "pve2"
        status = "offline"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve"
        uptime = 0

        [[storages]]
        storage = "fast"
        type = "lvmthin"
        content = "images,rootdir"

        [[volumes]]
        node = "pve1"
        storage = "fast"
        volid = "fast:vm-200-hs-home-user"
        size = 21474836480

        [[volumes]]
        node = "pve1"
        storage = "fast"
        volid = "fast:vm-299-hs-home-user"
        size = 1024

        [[vms]]
        vmid = 200
        node = "pve1"
        status = "running"
        config = { name = "test1", tags = "homestack-ws", memory = 4096, scsi1 = "fast:vm-200-hs-home-user,serial=HS_HOME_200,size=20G" }
        [vms.guest]
        hostname = "test1"
        users = ["user"]
        [[vms.guest.interfaces]]
        name = "eth0"
        ipv4 = ["192.0.2.200/24"]
        [[vms.guest.filesystems]]
        mountpoint = "/home/user"
        type = "ext4"
        device = "sdb"
        slot = "scsi1"
        label = "HS_HOME_200"
        size = 20957446144
        used = 338767872
        avail = 20601901056

        [[vms]]
        vmid = 201
        node = "pve1"
        status = "running"
        config = { name = "noagent" }

        [[vms]]
        vmid = 202
        node = "pve2"
        config = { name = "elsewhere" }

        [[vms]]
        vmid = 300
        node = "pve1"
        config = { name = "private" }

        [[tokens]]
        id = "root@pam!t"
        secret = "00000000-0000-0000-0000-000000000001"
        acl = { "/" = ["*"] }

        [[tokens]]
        id = "homestack@pve!server"
        secret = "00000000-0000-0000-0000-000000000002"
        acl = { "/nodes" = ["Sys.Audit"], "/vms/200" = ["VM.Audit", "VM.GuestAgent.Audit"], "/vms/201" = ["VM.Audit", "VM.GuestAgent.Audit"], "/vms/202" = ["VM.Audit"], "/storage" = ["Datastore.Audit"] }

        [[tokens]]
        id = "reader@pve!t"
        secret = "00000000-0000-0000-0000-000000000003"
        acl = { "/storage" = ["Datastore.Audit"], "/vms" = ["VM.Config.Disk"] }
        "#,
    )
}

async fn call(
    sim: &PveSim,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let ca = reqwest::Certificate::from_pem(sim.ca_pem().as_bytes())?;
    let client = reqwest::Client::builder().tls_certs_only([ca]).build()?;
    let mut request = client
        .request(method.parse()?, format!("{}/api2/json{path}", sim.base_url()))
        .header("Authorization", format!("PVEAPIToken={token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

async fn get(sim: &PveSim, token: &str, path: &str) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    call(sim, token, "GET", path, None).await
}

#[tokio::test]
async fn cluster_resources_list_only_the_vms_the_token_may_audit() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (_, body) = get(&sim, AUDITOR, "/cluster/resources?type=vm").await?;
    let vms: Vec<(u64, &str, &str)> = body["data"]
        .as_array()
        .ok_or("no list")?
        .iter()
        .filter_map(|vm| Some((vm["vmid"].as_u64()?, vm["status"].as_str()?, vm["name"].as_str()?)))
        .collect();
    assert_eq!(
        vms,
        [
            (200, "running", "test1"),
            (201, "running", "noagent"),
            (202, "unknown", "elsewhere")
        ]
    );
    assert_eq!(body["data"][0]["tags"], "homestack-ws");
    assert_eq!(body["data"][0]["type"], "qemu");
    assert!(body["data"][1].get("tags").is_none());
    Ok(())
}

#[tokio::test]
async fn vm_config_and_status_need_vm_audit() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = get(&sim, AUDITOR, "/nodes/pve1/qemu/200/config").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["memory"], 4096);
    assert_eq!(body["data"]["tags"], "homestack-ws");
    assert!(body["data"]["digest"].as_str().is_some_and(|d| d.len() == 40));

    let (status, body) = get(&sim, AUDITOR, "/nodes/pve1/qemu/200/status/current").await?;
    assert_eq!(
        (status, body["data"]["status"].as_str()),
        (StatusCode::OK, Some("running"))
    );

    let (status, body) = get(&sim, AUDITOR, "/nodes/pve1/qemu/300/config").await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["message"], "Permission check failed (/vms/300, VM.Audit)\n");

    let (status, body) = get(&sim, ROOT, "/nodes/pve1/qemu/299/config").await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body["message"],
        "Configuration file 'nodes/pve1/qemu-server/299.conf' does not exist\n"
    );
    let (status, _) = get(&sim, ROOT, "/nodes/pve1/qemu/202/config").await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "202 lives on pve2");
    let (status, _) = get(&sim, ROOT, "/nodes/pve2/qemu/202/config").await?;
    assert_eq!(status.as_u16(), 595);
    Ok(())
}

#[tokio::test]
async fn storages_and_capacity() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/storage").await?;
    assert_eq!(
        body["data"],
        json!([{"storage": "fast", "type": "lvmthin", "content": "images,rootdir", "enabled": 1, "active": 1, "shared": 0, "total": 1000, "used": 250, "avail": 750}])
    );
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/storage/fast/status").await?;
    assert_eq!(body["data"]["avail"], 750);
    assert!(body["data"].get("storage").is_none());
    let (status, body) = get(&sim, AUDITOR, "/nodes/pve1/storage/nosuch/status").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"]["storage"], "No such storage.");
    let (_, body) = get(&sim, AUDITOR, "/storage").await?;
    assert_eq!(body["data"][0]["storage"], "fast");
    Ok(())
}

#[tokio::test]
async fn vm_volumes_need_vm_config_disk_on_their_owner() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let path = "/nodes/pve1/storage/fast/content?content=images";
    let volids = |body: &Value| -> Vec<String> {
        body["data"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|v| v["volid"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (status, body) = get(&sim, AUDITOR, path).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        volids(&body).is_empty(),
        "Datastore.Audit alone shows no VM volume (observed live)"
    );

    let (_, body) = get(&sim, DISK_READER, path).await?;
    assert_eq!(volids(&body), ["fast:vm-200-hs-home-user", "fast:vm-299-hs-home-user"]);
    assert_eq!(body["data"][0]["vmid"], 200);
    assert_eq!(body["data"][0]["size"], 21_474_836_480_u64);

    let (status, body) = get(&sim, ROOT, "/nodes/pve1/storage/nosuch/content?content=images").await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["message"], "storage 'nosuch' does not exist\n");
    Ok(())
}

#[tokio::test]
async fn node_network_and_dns() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/network").await?;
    assert_eq!(body["data"][0]["iface"], "vmbr0");
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/dns").await?;
    assert_eq!(body["data"], json!({"dns1": "192.0.2.53", "search": "example"}));
    let (status, _) = get(&sim, DISK_READER, "/nodes/pve1/dns").await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn the_guest_agent_answers_audit_reads() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (status, body) = call(&sim, AUDITOR, "POST", "/nodes/pve1/qemu/200/agent/ping", None).await?;
    assert_eq!((status, &body), (StatusCode::OK, &json!({"data": {"result": {}}})));
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/qemu/200/agent/get-host-name").await?;
    assert_eq!(body["data"]["result"]["host-name"], "test1");
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/qemu/200/agent/network-get-interfaces").await?;
    assert_eq!(
        body["data"]["result"][1]["ip-addresses"][0]["ip-address"],
        "192.0.2.200"
    );
    let (_, body) = get(&sim, AUDITOR, "/nodes/pve1/qemu/200/agent/get-fsinfo").await?;
    assert_eq!(body["data"]["result"][0]["mountpoint"], "/home/user");

    let (status, body) = call(&sim, AUDITOR, "POST", "/nodes/pve1/qemu/201/agent/ping", None).await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["message"], "QEMU guest agent is not running\n");
    let (status, _) = call(&sim, ROOT, "POST", "/nodes/pve1/qemu/300/agent/ping", None).await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "VM 300 is stopped");

    let (status, _) = call(
        &sim,
        AUDITOR,
        "POST",
        "/nodes/pve1/qemu/200/agent/exec",
        Some(json!({"command": ["/bin/sh", "-lc", "hostname"]})),
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "exec needs VM.GuestAgent.Unrestricted");
    Ok(())
}

#[tokio::test]
async fn exec_runs_known_commands_and_is_recorded_as_operations() -> TestResult {
    let sim = PveSim::start(spec()?).await?;
    let (_, body) = call(
        &sim,
        ROOT,
        "POST",
        "/nodes/pve1/qemu/200/agent/exec",
        Some(json!({"command": ["/bin/sh", "-lc", "hostname"]})),
    )
    .await?;
    let pid = body["data"]["pid"].as_u64().ok_or("no pid")?;
    let (_, body) = get(&sim, ROOT, &format!("/nodes/pve1/qemu/200/agent/exec-status?pid={pid}")).await?;
    assert_eq!(body["data"], json!({"exited": 1, "exitcode": 0, "out-data": "test1\n"}));
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        ["guest_exec{cmd=hostname,vmid=200}", "guest_exec_status{vmid=200}"]
    );

    sim.clear_ops();
    call(&sim, ROOT, "POST", "/nodes/pve1/qemu/200/agent/ping", None).await?;
    get(&sim, ROOT, "/nodes/pve1/qemu/200/agent/get-fsinfo").await?;
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "guest_ping{vmid=200}",
            "get{path=/nodes/pve1/qemu/200/agent/get-fsinfo}"
        ]
    );
    Ok(())
}
