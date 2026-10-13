//! Phase 11 on the simulator (`docs/migration/phase-11.md`): two server
//! processes on one `pve-sim`.
//!
//! - The gate's test: a `refresh` of the same VM from both instances; one
//!   runs, the other gets `409 busy`, and the VM is left consistent. An
//!   instance that restarts takes its own stale lease over.
//! - An instance as the deploy command starts it: the client token as its
//!   SHA-256, the Proxmox token sent through `PUT /v1/secrets/pve-token`,
//!   the certificate from the environment, replaced through
//!   `PUT /v1/secrets/tls` with Caddy told to load it.

mod process;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use base64::Engine;
use hs_pve::{PveClient, PveClientConfig, PveToken};
use process::{API_TOKEN, Error, Launch, ServerProcess};
use pve_sim::scenario::FaultSpec;
use pve_sim::{ClusterSpec, PveSim};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIBfake\n-----END CERTIFICATE-----\n";
const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIBfake\n-----END PRIVATE KEY-----\n";

fn pve(sim: &PveSim) -> Result<PveClient, Error> {
    let mut config = PveClientConfig::new(vec![sim.base_url().parse()?], PveToken::parse(PVE_TOKEN)?);
    config.extra_ca_pem = Some(sim.ca_pem().to_owned());
    Ok(PveClient::new(config)?)
}

fn launch(sim: &PveSim, dir: &std::path::Path) -> Result<Launch, Error> {
    let token = dir.join("pve-token");
    std::fs::write(&token, PVE_TOKEN)?;
    Ok(Launch {
        endpoints: sim.base_url(),
        pve_token_file: token,
        ca_pem: Some(sim.ca_pem().to_owned()),
        pool: "homestack".to_owned(),
        gold_pool: "homestack-gold".to_owned(),
    })
}

async fn description(pve: &PveClient) -> Result<String, Error> {
    let config = pve.get_value(&["nodes", NODE, "qemu", "240", "config"], &[]).await?;
    Ok(config["description"].as_str().unwrap_or_default().to_owned())
}

async fn send(request: reqwest::RequestBuilder) -> Result<(StatusCode, Value), Error> {
    let response = request.bearer_auth(API_TOKEN).send().await?;
    let status = response.status();
    Ok((status, response.json().await.unwrap_or(Value::Null)))
}

async fn apply_raw(server: &ServerProcess, plan: &Value, version: &str) -> Result<(StatusCode, Value), Error> {
    let id = plan["id"].as_str().ok_or("no plan id")?;
    send(
        reqwest::Client::new()
            .post(format!("{}/v1/plans/{id}/apply", server.base))
            .header("homestack-config-version", version)
            .json(&json!({"digest": plan["digest"]})),
    )
    .await
}

#[tokio::test]
async fn a_refresh_from_two_instances_runs_once_and_the_other_gets_busy() -> Result<(), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let pve = pve(&sim)?;
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;
    let launch_a = launch(&sim, dir_a.path())?;
    let launch_b = launch(&sim, dir_b.path())?;
    let a = ServerProcess::start_with(&launch_a, dir_a.path(), &[("HOMESTACK_INSTANCE", "api1")]).await?;
    let b = ServerProcess::start_with(&launch_b, dir_b.path(), &[("HOMESTACK_INSTANCE", "api2")]).await?;
    let version = a.configure(SERVER_TOML).await?;
    assert_eq!(b.configure(SERVER_TOML).await?, version);

    let job = a
        .run(
            "workspace.create",
            json!({"vmid": 240, "name": "hst-two"}),
            &version,
            LIMIT,
        )
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(description(&pve).await?, "", "the create left no lease");

    // Both plan before either applies.
    let (status, plan_a) = a.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert_eq!(status, StatusCode::CREATED, "{plan_a}");
    let (status, plan_b) = b.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert_eq!(status, StatusCode::CREATED, "{plan_b}");

    // A's refresh stops at its first journal write (the second config
    // write; the first is its lease) and holds the VM.
    sim.inject_fault(FaultSpec {
        op: format!("put{{path=/nodes/{NODE}/qemu/240/config}}"),
        occurrence: 2,
        status: 500,
        message: String::new(),
        applied: false,
        hang: true,
        task: None,
    })?;
    let hangs = sim.hangs();
    let (status, job_a) = apply_raw(&a, &plan_a, &version).await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{job_a}");
    let deadline = tokio::time::Instant::now() + LIMIT;
    while sim.hangs() == hangs {
        if tokio::time::Instant::now() >= deadline {
            return Err("A's refresh never reached its journal".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let held = description(&pve).await?;
    assert!(held.contains("<!-- homestack-lease: "), "{held}");
    assert!(!held.contains("homestack-journal"), "{held}");
    let before = pve.get_value(&["nodes", NODE, "qemu", "240", "config"], &[]).await?;

    let (status, refused) = apply_raw(&b, &plan_b, &version).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "busy", "{refused}");
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("instance api1 runs workspace.refresh"), "{message}");
    assert_eq!(refused["error"]["retryable"], true, "{refused}");
    let (status, refused) = b.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert_eq!(
        (status, refused["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("busy")),
        "{refused}"
    );
    let (status, refused) = b.plan("workspace.destroy", json!({"vmid": 240})).await?;
    assert_eq!(
        (status, refused["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("busy")),
        "{refused}"
    );
    let after = pve.get_value(&["nodes", NODE, "qemu", "240", "config"], &[]).await?;
    assert_eq!(after, before, "B changed nothing");

    // A dies; started again under its name, it takes its stale lease over
    // and the refresh runs; B is free once A's job ended.
    a.kill()?;
    let a = ServerProcess::start_with(&launch_a, dir_a.path(), &[("HOMESTACK_INSTANCE", "api1")]).await?;
    let (status, refused) = b.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert_eq!(
        (status, refused["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("busy")),
        "{refused}"
    );
    let job = a
        .run("workspace.refresh", json!({"vmid": 240}), &version, LIMIT)
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(description(&pve).await?, "", "the lease and the journal are gone");
    let job = b
        .run("workspace.destroy", json!({"vmid": 240}), &version, LIMIT)
        .await?;
    assert_eq!(job["state"], "succeeded", "{job}");
    Ok(())
}

fn hex_sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn a_deployed_instance_gets_its_secrets_through_the_api() -> Result<(), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("state");
    let launch = launch(&sim, dir.path())?;
    let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
    let hash = hex_sha256(API_TOKEN);
    let env = [
        ("HOMESTACK_PVE_TOKEN_FILE", ""),
        ("HOMESTACK_API_TOKEN_FILE", ""),
        ("HOMESTACK_CONFIG_FILE", ""),
        ("HOMESTACK_API_TOKEN_SHA256", hash.as_str()),
        ("HOMESTACK_STATE_DIR", state.to_str().ok_or("path")?),
    ];
    let cert_env = [b64(CERT), b64(KEY)];
    let mut first = env.to_vec();
    first.push(("HOMESTACK_TLS_CERT_B64", &cert_env[0]));
    first.push(("HOMESTACK_TLS_KEY_B64", &cert_env[1]));
    let server = ServerProcess::start_with(&launch, dir.path(), &first).await?;
    let client = reqwest::Client::new();

    // The certificate of the environment is in the state directory.
    let tls = state.join("tls");
    assert_eq!(std::fs::read_to_string(tls.join("cert.pem"))?, CERT);
    assert_eq!(std::fs::read_to_string(tls.join("key.pem"))?, KEY);
    assert_eq!(
        std::fs::metadata(tls.join("key.pem"))?.permissions().mode() & 0o777,
        0o600
    );

    // Only the token whose hash it holds.
    let response = client
        .get(format!("{}/v1/config/version", server.base))
        .bearer_auth("x".repeat(40))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // No Proxmox token yet.
    let (status, body) = send(client.get(format!("{}/v1/pve/version", server.base))).await?;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("pve_token_missing")),
        "{body}"
    );
    let (status, body) = send(
        client
            .put(format!("{}/v1/secrets/pve-token", server.base))
            .body("not a token"),
    )
    .await?;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("validation_failed")),
        "{body}"
    );
    let (status, _) = send(
        client
            .put(format!("{}/v1/secrets/pve-token", server.base))
            .body(PVE_TOKEN),
    )
    .await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = send(client.get(format!("{}/v1/pve/version", server.base))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored = state.join("secrets").join("pve-token");
    assert_eq!(std::fs::read_to_string(&stored)?.trim(), PVE_TOKEN);
    assert_eq!(std::fs::metadata(&stored)?.permissions().mode() & 0o777, 0o600);
    // Write-only.
    let (status, _) = send(client.get(format!("{}/v1/secrets/pve-token", server.base))).await?;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    // The configuration goes to the state directory.
    let version = server.configure(SERVER_TOML).await?;
    assert!(state.join("server.toml").exists());

    // Started again with another certificate in the environment: the
    // stored files win, the token and the configuration are kept.
    server.kill()?;
    let other = [
        b64("-----BEGIN CERTIFICATE-----\nother\n-----END CERTIFICATE-----\n"),
        b64(KEY),
    ];
    let mut again = env.to_vec();
    again.push(("HOMESTACK_TLS_CERT_B64", &other[0]));
    again.push(("HOMESTACK_TLS_KEY_B64", &other[1]));
    let server = ServerProcess::start_with(&launch, dir.path(), &again).await?;
    assert_eq!(std::fs::read_to_string(tls.join("cert.pem"))?, CERT);
    let (status, body) = send(client.get(format!("{}/v1/pve/version", server.base))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(server.configure(SERVER_TOML).await?, version);
    Ok(())
}

/// A stand-in for Caddy's admin API: records requests, answers `GET
/// /config/` with a config and `POST /load` with `status`.
async fn fake_caddy(status: u16) -> Result<(String, Arc<Mutex<Vec<String>>>), Error> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
        let record = Arc::clone(&record);
        async move {
            let cache = headers
                .get("cache-control")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            if let Ok(mut seen) = record.lock() {
                seen.push(format!("{method} {uri} {cache} {}", String::from_utf8_lossy(&body)));
            }
            if method == Method::GET {
                (axum::http::StatusCode::OK, r#"{"apps":{"http":{}}}"#.to_owned())
            } else {
                (
                    axum::http::StatusCode::from_u16(status).unwrap_or_default(),
                    String::new(),
                )
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((format!("http://{addr}"), seen))
}

#[tokio::test]
async fn a_new_certificate_is_stored_and_caddy_loads_it() -> Result<(), Error> {
    let sim = PveSim::start(ClusterSpec::from_scenario(CLUSTER)?).await?;
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("state");
    let launch = launch(&sim, dir.path())?;
    let (admin, seen) = fake_caddy(200).await?;
    let state_text = state.to_str().ok_or("path")?;
    let server = ServerProcess::start_with(
        &launch,
        dir.path(),
        &[("HOMESTACK_STATE_DIR", state_text), ("HOMESTACK_CADDY_ADMIN", &admin)],
    )
    .await?;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/secrets/tls", server.base);

    let (status, body) = send(client.put(&url).json(&json!({"cert": "x", "key": KEY}))).await?;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("validation_failed")),
        "{body}"
    );
    let (status, body) = send(client.put(&url).json(&json!({"cert": CERT, "key": KEY}))).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(std::fs::read_to_string(state.join("tls").join("cert.pem"))?, CERT);
    let seen_now = seen.lock().map(|seen| seen.clone()).unwrap_or_default();
    assert_eq!(seen_now.len(), 2, "{seen_now:?}");
    assert!(seen_now[0].starts_with("GET /config/ "), "{seen_now:?}");
    assert!(
        seen_now[1].starts_with("POST /load must-revalidate {\"apps\":{\"http\":{}}}"),
        "{seen_now:?}"
    );
    drop(server);

    // Caddy refuses: the previous files come back.
    let (admin, _) = fake_caddy(400).await?;
    let server = ServerProcess::start_with(
        &launch,
        dir.path(),
        &[("HOMESTACK_STATE_DIR", state_text), ("HOMESTACK_CADDY_ADMIN", &admin)],
    )
    .await?;
    let newer = "-----BEGIN CERTIFICATE-----\nnewer\n-----END CERTIFICATE-----\n";
    let (status, body) = send(
        client
            .put(format!("{}/v1/secrets/tls", server.base))
            .json(&json!({"cert": newer, "key": KEY})),
    )
    .await?;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("validation_failed")),
        "{body}"
    );
    assert_eq!(std::fs::read_to_string(state.join("tls").join("cert.pem"))?, CERT);
    Ok(())
}
