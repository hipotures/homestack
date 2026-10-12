//! The Proxmox API client against the simulator.

use std::net::TcpListener;

use hs_model::NodeName;
use hs_pve::{PveClient, PveClientConfig, PveError, PveToken};
use pve_sim::{ClusterSpec, PveSim};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TOKEN: &str = "homestack@pve!server=11111111-2222-3333-4444-555555555555";

fn spec(extra: &str) -> Result<ClusterSpec, toml::de::Error> {
    toml::from_str(&format!(
        r#"
        version = {{ release = "9.2", version = "9.2.21", repoid = "4f6e0ac86f9e8c7f" }}

        [[nodes]]
        name = "node-a"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve"
        uptime = 1000

        [[nodes]]
        name = "node-b"
        status = "offline"
        pveversion = "pve-manager/9.2.21/4f6e0ac86f9e8c7f"
        kversion = "Linux 7.0.14-20-pve"
        uptime = 0

        [[tokens]]
        id = "homestack@pve!server"
        secret = "11111111-2222-3333-4444-555555555555"
        acl = {{ "/" = ["Sys.Audit"] }}

        [[tokens]]
        id = "weak@pve!t"
        secret = "22222222-2222-3333-4444-555555555555"
        acl = {{ "/vms" = ["VM.Audit"] }}
        {extra}
        "#
    ))
}

fn config(endpoints: &[String], token: &str, ca_pem: &str) -> Result<PveClientConfig, Box<dyn std::error::Error>> {
    let endpoints = endpoints.iter().map(|e| e.parse()).collect::<Result<Vec<_>, _>>()?;
    let mut config = PveClientConfig::new(endpoints, PveToken::parse(token)?);
    config.extra_ca_pem = Some(ca_pem.to_owned());
    Ok(config)
}

/// A loopback URL nothing listens on.
fn closed_port_url() -> Result<String, std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(format!("https://localhost:{port}"))
}

#[test]
fn token_parsing_accepts_only_the_pve_format_and_never_shows_the_secret() -> TestResult {
    let token = PveToken::parse(&format!("  {TOKEN}\n"))?;
    assert_eq!(token.id(), "homestack@pve!server");
    let debug = format!("{token:?}");
    assert!(!debug.contains("11111111"), "{debug}");

    for bad in [
        "",
        "homestack@pve!server",
        "homestack@pve!server=",
        "homestack!server=s",
        "homestack@pve=s",
        "a@b!c=se cret",
        "@pve!t=s",
        "u@!t=s",
        "u@pve!=s",
    ] {
        let error = PveToken::parse(bad).err().ok_or(format!("accepted {bad:?}"))?;
        assert!(!error.to_string().contains("cret"), "{error}");
    }
    Ok(())
}

#[test]
fn endpoints_must_be_plain_https_origins() -> TestResult {
    let token = PveToken::parse(TOKEN)?;
    for bad in [
        "http://pve1.example:8006",
        "https://user@pve1.example:8006",
        "https://pve1.example:8006/api2",
        "https://pve1.example:8006/?x=1",
    ] {
        let config = PveClientConfig::new(vec![bad.parse()?], token.clone());
        assert!(matches!(PveClient::new(config), Err(PveError::Config(_))), "{bad}");
    }
    let empty = PveClientConfig::new(Vec::new(), token);
    assert!(matches!(PveClient::new(empty), Err(PveError::Config(_))));
    Ok(())
}

#[tokio::test]
async fn reads_version_nodes_and_node_status() -> TestResult {
    let sim = PveSim::start(spec("")?).await?;
    let client = PveClient::new(config(&[sim.base_url()], TOKEN, sim.ca_pem())?)?;

    let version = client.version().await?;
    assert_eq!((version.release.as_str(), version.version.as_str()), ("9.2", "9.2.21"));

    let nodes = client.nodes().await?;
    let names: Vec<(&str, bool)> = nodes.iter().map(|n| (n.node.as_str(), n.online())).collect();
    assert_eq!(names, [("node-a", true), ("node-b", false)]);

    let status = client.node_status(&NodeName::parse("node-a")?).await?;
    assert_eq!(status.node, "node-a");
    assert_eq!(status.pveversion, "pve-manager/9.2.21/4f6e0ac86f9e8c7f");
    assert_eq!(status.uptime, 1000);

    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "get{path=/version}",
            "get{path=/nodes}",
            "get{path=/nodes/node-a/status}"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn fails_over_when_an_endpoint_cannot_be_connected() -> TestResult {
    let sim = PveSim::start(spec("")?).await?;
    let client = PveClient::new(config(&[closed_port_url()?, sim.base_url()], TOKEN, sim.ca_pem())?)?;
    client.version().await?;
    client.version().await?;
    assert_eq!(sim.ops().len(), 2);
    Ok(())
}

#[tokio::test]
async fn an_endpoint_with_an_untrusted_certificate_never_receives_the_token() -> TestResult {
    let untrusted = PveSim::start(spec("")?).await?;
    let trusted = PveSim::start(spec("")?).await?;
    let client = PveClient::new(config(
        &[untrusted.base_url(), trusted.base_url()],
        TOKEN,
        trusted.ca_pem(),
    )?)?;
    client.version().await?;
    assert!(
        untrusted.ops().is_empty(),
        "the untrusted endpoint saw an authenticated call"
    );
    assert_eq!(trusted.ops().len(), 1);

    let only_untrusted = PveClient::new(config(&[untrusted.base_url()], TOKEN, trusted.ca_pem())?)?;
    match only_untrusted.version().await {
        Err(PveError::Unreachable(failures)) => assert_eq!(failures.len(), 1),
        other => return Err(format!("expected Unreachable, got {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn reports_unreachable_when_no_endpoint_answers() -> TestResult {
    let client = PveClient::new(config(&[closed_port_url()?, closed_port_url()?], TOKEN, "")?)?;
    match client.version().await {
        Err(PveError::Unreachable(failures)) => assert_eq!(failures.len(), 2),
        other => return Err(format!("expected Unreachable, got {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn an_http_error_is_returned_without_trying_another_endpoint() -> TestResult {
    let extra = r#"
        [[faults]]
        op = "get{path=/version}"
        occurrence = 1
        status = 500
        message = "injected\n"
    "#;
    let first = PveSim::start(spec(extra)?).await?;
    let second = PveSim::start(spec("")?).await?;
    let mut config = config(&[first.base_url(), second.base_url()], TOKEN, first.ca_pem())?;
    config.extra_ca_pem = Some(format!("{}{}", first.ca_pem(), second.ca_pem()));
    let client = PveClient::new(config)?;
    match client.version().await {
        Err(PveError::Api { status: 500, message }) => assert_eq!(message, "injected"),
        other => return Err(format!("expected Api 500, got {other:?}").into()),
    }
    assert!(second.ops().is_empty());
    Ok(())
}

#[tokio::test]
async fn maps_pve_error_statuses() -> TestResult {
    let sim = PveSim::start(spec("")?).await?;
    let node_a = NodeName::parse("node-a")?;

    let wrong_secret = PveClient::new(config(&[sim.base_url()], "homestack@pve!server=nope", sim.ca_pem())?)?;
    assert!(matches!(wrong_secret.version().await, Err(PveError::Unauthorized)));

    let weak = PveClient::new(config(
        &[sim.base_url()],
        "weak@pve!t=22222222-2222-3333-4444-555555555555",
        sim.ca_pem(),
    )?)?;
    match weak.node_status(&node_a).await {
        Err(PveError::Forbidden { message }) => {
            assert_eq!(message, "Permission check failed (/nodes/node-a, Sys.Audit)")
        }
        other => return Err(format!("expected Forbidden, got {other:?}").into()),
    }

    let client = PveClient::new(config(&[sim.base_url()], TOKEN, sim.ca_pem())?)?;
    assert!(matches!(
        client.node_status(&NodeName::parse("node-b")?).await,
        Err(PveError::NodeUnreachable { .. })
    ));
    assert!(matches!(
        client.node_status(&NodeName::parse("nosuchnode")?).await,
        Err(PveError::Api { status: 500, .. })
    ));
    Ok(())
}

#[tokio::test]
async fn reads_the_effective_privileges_on_a_path() -> TestResult {
    let sim = PveSim::start(spec("")?).await?;
    let client = PveClient::new(config(&[sim.base_url()], TOKEN, sim.ca_pem())?)?;
    let privileges = client.permissions("/nodes/node-a").await?;
    assert_eq!(privileges.into_iter().collect::<Vec<_>>(), ["Sys.Audit"]);
    // An ACL entry on `/` covers every path below it, as on PVE.
    assert_eq!(
        client.permissions("/vms/100").await?.into_iter().collect::<Vec<_>>(),
        ["Sys.Audit"]
    );

    let weak = PveClient::new(config(
        &[sim.base_url()],
        "weak@pve!t=22222222-2222-3333-4444-555555555555",
        sim.ca_pem(),
    )?)?;
    assert!(weak.permissions("/nodes").await?.is_empty());
    assert_eq!(
        weak.permissions("/vms/100").await?.into_iter().collect::<Vec<_>>(),
        ["VM.Audit"]
    );
    let ops: Vec<String> = sim.ops().iter().map(ToString::to_string).collect();
    assert_eq!(
        ops,
        [
            "get{path=/access/permissions,query_path=/nodes/node-a}",
            "get{path=/access/permissions,query_path=/vms/100}",
            "get{path=/access/permissions,query_path=/nodes}",
            "get{path=/access/permissions,query_path=/vms/100}",
        ]
    );
    Ok(())
}
