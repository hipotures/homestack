//! The server as its own process (`homestack-server`), so a test can kill
//! it in the middle of an operation and start it again on the same stored
//! configuration (the gate of phase 6). Shared by `refresh_process.rs`
//! (simulator) and `live_refresh.rs` (cluster).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};

pub type Error = Box<dyn std::error::Error>;

pub const API_TOKEN: &str = "process-api-token-0123456789abcdef0123456789";

/// What the server process is started with.
pub struct Launch {
    pub endpoints: String,
    pub pve_token_file: PathBuf,
    pub ca_pem: Option<String>,
    pub pool: String,
    pub gold_pool: String,
}

/// A running `homestack-server`; killed when dropped.
pub struct ServerProcess {
    child: Child,
    pub base: String,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> Result<u16, Error> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

impl ServerProcess {
    /// Start the server with its files in `dir` (the stored `server.toml`
    /// survives a restart there) and wait until it answers.
    pub async fn start(launch: &Launch, dir: &Path) -> Result<Self, Error> {
        Self::start_with(launch, dir, &[]).await
    }

    /// [`Self::start`] with more variables; an empty value unsets one.
    pub async fn start_with(launch: &Launch, dir: &Path, extra: &[(&str, &str)]) -> Result<Self, Error> {
        let api_token = dir.join("api-token");
        std::fs::write(&api_token, API_TOKEN)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_homestack-server"));
        let port = free_port()?;
        command
            .env_clear()
            .env("HOMESTACK_LISTEN", format!("127.0.0.1:{port}"))
            .env("HOMESTACK_API_TOKEN_FILE", &api_token)
            .env("HOMESTACK_PVE_ENDPOINTS", &launch.endpoints)
            .env("HOMESTACK_PVE_TOKEN_FILE", &launch.pve_token_file)
            .env("HOMESTACK_CONFIG_FILE", dir.join("server.toml"))
            .env("HOMESTACK_PVE_POOL", &launch.pool)
            .env("HOMESTACK_PVE_GOLD_POOL", &launch.gold_pool)
            .env("HOMESTACK_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(pem) = &launch.ca_pem {
            let ca = dir.join("ca.pem");
            std::fs::write(&ca, pem)?;
            command.env("HOMESTACK_PVE_CA_FILE", ca);
        }
        for (name, value) in extra {
            if value.is_empty() {
                command.env_remove(name);
            } else {
                command.env(name, value);
            }
        }
        let child = command.spawn()?;
        let server = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
        };
        let client = reqwest::Client::new();
        for _ in 0..200 {
            if let Ok(response) = client.get(format!("{}/health/live", server.base)).send().await
                && response.status().is_success()
            {
                return Ok(server);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("homestack-server did not start".into())
    }

    /// Kill the process at once (`SIGKILL`), as a crash or `kill -9` does.
    pub fn kill(mut self) -> Result<(), Error> {
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<(StatusCode, Value), Error> {
        let response = request.bearer_auth(API_TOKEN).send().await?;
        let status = response.status();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }

    /// Upload `server.toml` unless one is stored; its version.
    pub async fn configure(&self, toml: &str) -> Result<String, Error> {
        let client = reqwest::Client::new();
        let (status, body) = self
            .send(
                client
                    .put(format!("{}/v1/config", self.base))
                    .header("if-none-match", "*")
                    .body(toml.to_owned()),
            )
            .await?;
        if !(status.is_success() || status == StatusCode::PRECONDITION_FAILED) {
            return Err(format!("config upload: {status} {body}").into());
        }
        let (status, body) = self
            .send(client.get(format!("{}/v1/config/version", self.base)))
            .await?;
        if !status.is_success() {
            return Err(format!("config version: {status} {body}").into());
        }
        body.get("config_version")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("config version: {body}").into())
    }

    pub async fn plan(&self, operation: &str, parameters: Value) -> Result<(StatusCode, Value), Error> {
        self.send(
            reqwest::Client::new()
                .post(format!("{}/v1/plans", self.base))
                .json(&json!({"operation": operation, "parameters": parameters})),
        )
        .await
    }

    /// Apply a plan; the job as accepted.
    pub async fn apply(&self, plan: &Value, version: &str) -> Result<Value, Error> {
        let id = plan["id"].as_str().ok_or("no plan id")?;
        let (status, job) = self
            .send(
                reqwest::Client::new()
                    .post(format!("{}/v1/plans/{id}/apply", self.base))
                    .header("homestack-config-version", version)
                    .json(&json!({"digest": plan["digest"]})),
            )
            .await?;
        if status != StatusCode::ACCEPTED {
            return Err(format!("apply: {status} {job}").into());
        }
        Ok(job)
    }

    /// Wait for a job to end, at most `limit`.
    pub async fn finished(&self, job: &Value, limit: Duration) -> Result<Value, Error> {
        let id = job["id"].as_str().ok_or("no job id")?;
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let (_, job) = self
                .send(reqwest::Client::new().get(format!("{}/v1/jobs/{id}", self.base)))
                .await?;
            if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
                return Ok(job);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("job {id} did not finish: {job}").into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Plan and apply `operation`, and wait for its job.
    pub async fn run(
        &self,
        operation: &str,
        parameters: Value,
        version: &str,
        limit: Duration,
    ) -> Result<Value, Error> {
        let (status, plan) = self.plan(operation, parameters).await?;
        if status != StatusCode::CREATED {
            return Err(format!("plan {operation}: {status} {plan}").into());
        }
        let job = self.apply(&plan, version).await?;
        self.finished(&job, limit).await
    }
}
