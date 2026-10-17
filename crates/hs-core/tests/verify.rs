//! Guest verification (D2) at `VM.GuestAgent.Audit` + `FileRead`
//! (decision 5) against the simulated guests of
//! `scenarios/guest-checks.toml`.
//!
//! Python's checks live inline in `lifecycle.create_workspace` and in
//! `lifecycle._verify_refreshed_guest`; each failure below carries both
//! messages. The refresh variant is also compared with Python in the parity
//! test.

use std::path::Path;

use hs_core::verify::{GuestExpectation, VerifyFailure, VerifyTimeouts, verify_guest};
use hs_model::config::User;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use pve_sim::{ClusterSpec, PveSim};

const NODE: &str = "pve-example-1";
const SERVER_TOKEN: &str = "homestack@pve!server=00000000-0000-0000-0000-00000000000b";
const AUDITOR_TOKEN: &str = "auditor@pve!server=00000000-0000-0000-0000-00000000000c";

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn cluster() -> Result<PveSim, Box<dyn std::error::Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/guest-checks.toml");
    let spec = ClusterSpec::from_scenario(&std::fs::read_to_string(path)?)?;
    Ok(PveSim::start(spec).await?)
}

fn client(sim: &PveSim, token: &str) -> Result<PveClient, Box<dyn std::error::Error>> {
    let mut config = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(token)?);
    config.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(config)?)
}

fn expected(vmid: u32) -> GuestExpectation {
    GuestExpectation {
        name: format!("ws{vmid}"),
        address: format!("192.0.2.{vmid}/24"),
        user: User {
            name: "developer".to_owned(),
            uid: 1000,
            gid: 1000,
        },
        home_label: format!("HS_HOME_{vmid}"),
    }
}

fn quick() -> VerifyTimeouts {
    VerifyTimeouts {
        agent: std::time::Duration::from_secs(5),
        cloud_init: std::time::Duration::from_secs(5),
    }
}

async fn failure(pve: &PveClient, vmid: u32) -> Result<VerifyFailure, Box<dyn std::error::Error>> {
    match verify_guest(pve, NODE, vmid, &expected(vmid), &quick()).await? {
        Ok(verified) => Err(format!("VM {vmid} verified: {verified:?}").into()),
        Err(failure) => Ok(failure),
    }
}

#[tokio::test]
async fn a_healthy_guest_passes_and_reports_cloud_id_and_keys() -> TestResult {
    let sim = cluster().await?;
    let pve = client(&sim, SERVER_TOKEN)?;
    let verified = verify_guest(&pve, NODE, 220, &expected(220), &quick()).await?;
    let verified = verified.map_err(|f| f.refresh_message())?;
    assert_eq!(verified.cloud_id.as_deref(), Some("nocloud"));
    assert_eq!(
        verified.user_authorized_keys,
        "ssh-ed25519 AAAAUSER developer@desktop\n"
    );
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert!(
        ops.iter().all(|op| !op.starts_with("guest_exec")),
        "no exec at audit level: {ops:?}"
    );
    Ok(())
}

#[tokio::test]
async fn installed_sudo_does_not_block_workspace_verification() -> TestResult {
    let sim = cluster().await?;
    let pve = client(&sim, SERVER_TOKEN)?;
    let result = verify_guest(&pve, NODE, 223, &expected(223), &quick()).await?;
    assert!(result.is_ok(), "{result:?}");
    assert!(sim.ops().iter().all(|op| !op.to_string().starts_with("guest_exec")));
    Ok(())
}

#[tokio::test]
async fn every_broken_step_has_pythons_create_and_refresh_message() -> TestResult {
    let sim = cluster().await?;
    let pve = client(&sim, SERVER_TOKEN)?;
    let cases: [(u32, &str, &str); 8] = [
        (
            221,
            "Verification failed: hostname is 'ubuntu', expected 'ws221'",
            "Refresh verification failed: workspace hostname is incorrect",
        ),
        (
            222,
            "Verification failed: user authorized_keys is missing",
            "Refresh verification failed: persistent user authorized_keys is missing",
        ),
        (
            224,
            "Verification failed: unexpected regular user accounts: developer:1000:1000, intruder:1001:1001",
            "Refresh verification failed: unexpected regular user accounts",
        ),
        (
            225,
            "Verification failed: eth0 does not have 192.0.2.225/24\n192.0.2.99/24",
            "Refresh verification failed: eth0 does not have 192.0.2.225/24",
        ),
        (
            226,
            "Verification failed: /home/developer is not mounted as ext4\n",
            "Refresh verification failed: /home/developer is not ext4",
        ),
        (
            227,
            "Verification failed: persistent home label is 'HS_HOME_999', expected 'HS_HOME_227'",
            "Refresh verification failed: home label is 'HS_HOME_999'",
        ),
        (
            228,
            "Verification failed: root authorized_keys is missing",
            "Refresh verification failed: root authorized_keys is missing",
        ),
        (
            229,
            "Verification failed: unexpected user identity: developer:x:1001:1001::/home/developer:/bin/bash",
            "Refresh verification failed: unexpected user identity: developer:x:1001:1001::/home/developer:/bin/bash",
        ),
    ];
    for (vmid, create, refresh) in cases {
        let found = failure(&pve, vmid).await?;
        assert_eq!(found.create_message(&expected(vmid)), create, "VM {vmid}");
        assert_eq!(found.refresh_message(), refresh, "VM {vmid}");
    }
    Ok(())
}

#[tokio::test]
async fn without_file_read_the_verification_names_the_privilege() -> TestResult {
    let sim = cluster().await?;
    let pve = client(&sim, AUDITOR_TOKEN)?;
    let error = verify_guest(&pve, NODE, 220, &expected(220), &quick())
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(error.contains("VM.GuestAgent.FileRead"), "{error}");
    Ok(())
}
