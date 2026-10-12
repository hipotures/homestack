//! Uploading the server configuration: `PUT /v1/config` with `If-Match`,
//! `GET /v1/config`, `GET /v1/config/version` (mapping 8.2). The store
//! keeps the guarantees of Python's `config.publish_config`.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::{API_TOKEN, PVE_TOKEN, SERVER_TOML, Server, TestResult, assert_error, etag, spec};
use hs_model::config::config_version;
use pve_sim::PveSim;
use reqwest::{Method, StatusCode};
use serde_json::json;

async fn sim() -> Result<PveSim, common::Error> {
    Ok(PveSim::start(spec(r#""/" = ["Sys.Audit"]"#)?).await?)
}

#[tokio::test]
async fn a_new_server_has_no_configuration() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;

    let (status, headers, body) = server.get("/v1/config", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&body, &headers, "not_found", false);

    let (status, _, body) = server.get("/v1/config/version", Some(API_TOKEN)).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"config_version": null}));
    assert!(!server.config_file.exists());
    assert!(sim.ops().is_empty(), "configuration routes never call Proxmox");
    Ok(())
}

/// Includes the port of `test_config.py::ConfigTests::test_publish_new_config_uses_private_mode`.
#[tokio::test]
async fn the_first_upload_needs_if_none_match_and_is_private() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    let config_dir = dir.path().join("state/homestack");
    let server = Server::start_with(&sim, &[sim.base_url()], PVE_TOKEN, &config_dir.join("server.toml")).await?;

    let reply = server
        .send(Method::PUT, "/v1/config", Some(API_TOKEN), &[], Some(SERVER_TOML))
        .await?;
    assert_eq!(reply.status, StatusCode::PRECONDITION_REQUIRED);
    assert_error(&reply.json(), &reply.headers, "precondition_required", false);

    let reply = server.put_config(SERVER_TOML, ("if-match", &etag("0"))).await?;
    assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED);
    assert_error(&reply.json(), &reply.headers, "precondition_failed", false);
    assert!(!server.config_file.exists());

    let version = config_version(SERVER_TOML);
    let reply = server.put_config(SERVER_TOML, ("if-none-match", "*")).await?;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json(), json!({"config_version": version, "changed": true}));
    assert_eq!(reply.header("etag"), Some(etag(&version).as_str()));

    assert_eq!(std::fs::read_to_string(&server.config_file)?, SERVER_TOML);
    assert_eq!(
        std::fs::metadata(&server.config_file)?.permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::metadata(&config_dir)?.permissions().mode() & 0o777, 0o700);
    let leftovers: Vec<_> = std::fs::read_dir(&config_dir)?
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert_eq!(leftovers, ["server.toml"], "no temporary or backup files");

    let reply = server
        .send(Method::GET, "/v1/config", Some(API_TOKEN), &[], None)
        .await?;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text, SERVER_TOML);
    assert_eq!(reply.header("etag"), Some(etag(&version).as_str()));
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/toml"))
    );
    let (_, _, body) = server.get("/v1/config/version", Some(API_TOKEN)).await?;
    assert_eq!(body, json!({"config_version": version}));

    let reply = server.put_config(SERVER_TOML, ("if-none-match", "*")).await?;
    assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED);
    assert!(sim.ops().is_empty());
    Ok(())
}

/// Port of `test_config.py::ConfigTests::test_publish_existing_config_creates_backup`.
#[tokio::test]
async fn replacing_needs_the_current_version_and_keeps_a_backup() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    let old = config_version(SERVER_TOML);
    server.put_config(SERVER_TOML, ("if-none-match", "*")).await?;

    let newer = SERVER_TOML.replace("gold_vmid = 100", "gold_vmid = 102");
    let reply = server
        .put_config(&newer, ("if-match", &etag(&config_version("stale"))))
        .await?;
    assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(std::fs::read_to_string(&server.config_file)?, SERVER_TOML);

    let reply = server.put_config(&newer, ("if-match", &etag(&old))).await?;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json()["config_version"], config_version(&newer));
    assert_eq!(std::fs::read_to_string(&server.config_file)?, newer);

    let backups: Vec<_> = std::fs::read_dir(dir.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".backup-"))
        })
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(std::fs::read_to_string(&backups[0])?, SERVER_TOML);
    assert_eq!(std::fs::metadata(&backups[0])?.permissions().mode() & 0o777, 0o600);

    // The same text again changes nothing and writes nothing.
    let reply = server
        .put_config(&newer, ("if-match", &etag(&config_version(&newer))))
        .await?;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["changed"], false);
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 2);

    // A wildcard If-Match needs an existing configuration, then accepts any.
    let reply = server.put_config(SERVER_TOML, ("if-match", "*")).await?;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["config_version"], old);
    Ok(())
}

/// Port of `test_config.py::ConfigTests::test_validation_failure_never_replaces_existing_config`.
#[tokio::test]
async fn an_invalid_configuration_never_replaces_the_current_one() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    let version = config_version(SERVER_TOML);
    server.put_config(SERVER_TOML, ("if-none-match", "*")).await?;

    let without_layouts = SERVER_TOML
        .replace("homestack-storage-1 = [\"fast\", \"bulk\"]\n", "")
        .replace("homestack-storage-2 = [\"other\"]\n", "");
    let secretish = SERVER_TOML
        .replace("owner", "x")
        .replace("[user]", "[user]\npassword = \"hunter2-secret\"");
    for (text, field) in [
        (without_layouts.as_str(), "storage_layouts"),
        (secretish.as_str(), "user.password"),
        ("not = [toml", "toml"),
    ] {
        let reply = server.put_config(text, ("if-match", &etag(&version))).await?;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{field}");
        let body = reply.json();
        assert_error(&body, &reply.headers, "config_invalid", false);
        assert_eq!(body["error"]["field"], field, "{body}");
        assert!(!reply.text.contains("hunter2"), "input echoed: {body}");
    }
    let reply = server
        .put_config("\u{feff}\u{0}", ("if-match", &etag(&version)))
        .await?;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(std::fs::read_to_string(&server.config_file)?, SERVER_TOML);
    let (_, _, body) = server.get("/v1/config/version", Some(API_TOKEN)).await?;
    assert_eq!(body["config_version"], version);
    Ok(())
}

#[tokio::test]
async fn the_configuration_survives_a_restart() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    server.put_config(SERVER_TOML, ("if-none-match", "*")).await?;
    server.stop();

    let restarted = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    let reply = restarted
        .send(Method::GET, "/v1/config", Some(API_TOKEN), &[], None)
        .await?;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text, SERVER_TOML);
    let (_, _, body) = restarted.get("/v1/config/version", Some(API_TOKEN)).await?;
    assert_eq!(body["config_version"], config_version(SERVER_TOML));
    Ok(())
}

#[tokio::test]
async fn an_invalid_file_on_disk_stops_the_server_from_starting() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    std::fs::write(
        dir.path().join("server.toml"),
        SERVER_TOML.replace("cidr = 24", "cidr = 64"),
    )?;
    let started = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await;
    let error = match started {
        Ok(_) => return Err("started with an invalid configuration".into()),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("network.cidr"), "{error}");
    Ok(())
}

#[tokio::test]
async fn configuration_routes_need_the_token_and_limit_the_body() -> TestResult {
    let dir = tempfile::tempdir()?;
    let sim = sim().await?;
    let server = Server::start(&sim, &[sim.base_url()], PVE_TOKEN, dir.path()).await?;
    for (method, path) in [
        (Method::GET, "/v1/config"),
        (Method::GET, "/v1/config/version"),
        (Method::PUT, "/v1/config"),
    ] {
        let reply = server
            .send(method, path, None, &[("if-none-match", "*")], Some(SERVER_TOML))
            .await?;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    assert!(!server.config_file.exists());

    let huge = format!("{SERVER_TOML}\n# {}\n", "x".repeat(300 * 1024));
    let reply = server.put_config(&huge, ("if-none-match", "*")).await?;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_error(&reply.json(), &reply.headers, "payload_too_large", false);
    assert!(!server.config_file.exists());
    Ok(())
}
