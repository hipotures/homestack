//! Security safeguard: QGA transport diagnostics must not enter public jobs.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use hs_core::guest::{guest_exec, read_text_or_reason};
use hs_core::pve::Pve;
use hs_core::resize_guest::{GuestSystem, QgaGuest};
use hs_pve::PveError;
use hs_server::operations::{BoxFuture, JobHandle, OpContext, OpFailure, Operation, Planned};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const SENTINEL: &str = "PRIVATE-PVE-DIAGNOSTIC-RA02";
const TOKEN: &str = "test-api-token-0123456789abcdef0123456789";
#[derive(Clone)]
struct Fault {
    error: PveError,
    status: bool,
}
impl Pve for Fault {
    async fn get(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        Err(self.error.clone())
    }
    async fn post(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        if self.status {
            Ok(json!({"pid":42}))
        } else {
            Err(self.error.clone())
        }
    }
    async fn put(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        unreachable!()
    }
    async fn delete(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        unreachable!()
    }
}
struct Probe {
    fault: Fault,
    planning: bool,
}
impl Operation for Probe {
    fn name(&self) -> &'static str {
        "diagnostic.resize"
    }
    fn plan<'a>(&'a self, _: &'a OpContext, _: &'a Value) -> BoxFuture<'a, Result<Planned, OpFailure>> {
        Box::pin(async move {
            if self.planning {
                // The actual resize inspector's tool probe uses unchecked execution.
                QgaGuest::new(&self.fault, "node", 120).has_tool("growpart").await?;
            }
            Ok(Planned {
                targets: vec![],
                summary: json!({}),
            })
        })
    }
    fn execute<'a>(
        &'a self,
        _: &'a OpContext,
        _: &'a Planned,
        _: &'a JobHandle,
    ) -> BoxFuture<'a, Result<Value, OpFailure>> {
        Box::pin(async move {
            let _ = QgaGuest::new(&self.fault, "node", 120)
                .run(&["growpart", "/dev/sda", "1"])
                .await?;
            Ok(Value::Null)
        })
    }
}

#[tokio::test]
async fn unchecked_qga_failures_are_fixed_in_http_jobs_lists_and_websockets() {
    let errors = [
        (
            PveError::Api {
                status: 500,
                message: SENTINEL.into(),
            },
            "pve_error",
        ),
        (
            PveError::NodeUnreachable {
                message: SENTINEL.into(),
            },
            "pve_node_unavailable",
        ),
        (PveError::Response(SENTINEL.into()), "pve_error"),
        (
            PveError::Forbidden {
                message: SENTINEL.into(),
            },
            "pve_permission_denied",
        ),
        (PveError::Unauthorized, "pve_auth_failed"),
        (
            PveError::Unreachable(vec![hs_pve::EndpointFailure {
                endpoint: "https://localhost:1".into(),
                detail: SENTINEL.into(),
            }]),
            "pve_unavailable",
        ),
    ];
    for (error, code) in errors {
        for status in [false, true] {
            let fault = Fault {
                error: error.clone(),
                status,
            };
            // The adjacent file-read adapter must retain the same infrastructure cause.
            assert!(
                read_text_or_reason(&fault, "node", 120, "/etc/crypttab")
                    .await
                    .unwrap_err()
                    .pve_error()
                    .is_some()
            );
            for planning in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let pve = hs_pve::PveClientConfig::new(
                    vec!["https://localhost:1".parse().unwrap()],
                    hs_pve::PveToken::parse("test@pve!test=synthetic").unwrap(),
                );
                let app = hs_server::app_with(
                    hs_server::ServerConfig {
                        heartbeat: Arc::default(),
                        api_token: hs_server::ApiToken::parse(TOKEN).unwrap().into(),
                        pve: pve.into(),
                        config_file: dir.path().join("server.toml"),
                        instance: Default::default(),
                        pools: Default::default(),
                    },
                    hs_server::AppOptions {
                        operations: vec![Arc::new(Probe {
                            fault: fault.clone(),
                            planning,
                        })],
                        ..Default::default()
                    },
                )
                .unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let task = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let http = reqwest::Client::new();
                let plan = http
                    .post(format!("{base}/v1/plans"))
                    .bearer_auth(TOKEN)
                    .json(&json!({"operation":"diagnostic.resize"}))
                    .send()
                    .await
                    .unwrap();
                if planning {
                    assert!(plan.status().is_client_error() || plan.status().is_server_error());
                    let body: Value = plan.json().await.unwrap();
                    assert_eq!(body["error"]["code"], code);
                    assert!(!body.to_string().contains(SENTINEL));
                } else {
                    assert_eq!(plan.status(), 201);
                    let plan: Value = plan.json().await.unwrap();
                    let reply = http
                        .post(format!("{base}/v1/plans/{}/apply", plan["id"].as_str().unwrap()))
                        .bearer_auth(TOKEN)
                        .json(&json!({"digest":plan["digest"]}))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(reply.status(), 202);
                    let job: Value = reply.json().await.unwrap();
                    let id = job["id"].as_str().unwrap();
                    let mut request = format!("{}/v1/jobs/{id}/events", base.replace("http:", "ws:"))
                        .into_client_request()
                        .unwrap();
                    request
                        .headers_mut()
                        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
                    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while let Some(message) = ws.next().await {
                            let message = message.unwrap();
                            if message.is_text() {
                                let text = message.to_text().unwrap();
                                assert!(!text.contains(SENTINEL));
                                let value: Value = serde_json::from_str(text).unwrap();
                                if value["data"]["error"].is_object() {
                                    assert_eq!(value["data"]["error"]["code"], code);
                                }
                            }
                        }
                    })
                    .await
                    .unwrap();
                    for path in [format!("/v1/jobs/{id}"), "/v1/jobs".to_owned()] {
                        let body: Value = http
                            .get(format!("{base}{path}"))
                            .bearer_auth(TOKEN)
                            .send()
                            .await
                            .unwrap()
                            .json()
                            .await
                            .unwrap();
                        assert!(!body.to_string().contains(SENTINEL));
                        let job = if body.is_array() { &body[0] } else { &body };
                        assert_eq!(job["error"]["code"], code);
                    }
                }
                task.abort();
            }
        }
    }
}

struct Completed(Value);
impl Pve for Completed {
    async fn post(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        Ok(self.0.clone())
    }
    async fn get(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        unreachable!()
    }
    async fn put(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        unreachable!()
    }
    async fn delete(&self, _: &str, _: &[(&str, &str)]) -> Result<Value, PveError> {
        unreachable!()
    }
}
#[tokio::test]
async fn genuine_guest_exits_and_nochange_keep_stdout_and_stderr() {
    for (exit, out, err) in [
        (0, "grown", ""),
        (1, "NOCHANGE: partition is size 100", "diagnostic"),
        (1, "stdout", "stderr"),
    ] {
        let pve = Completed(json!({"exited":true,"exitcode":exit,"out-data":out,"err-data":err}));
        let result = guest_exec(&pve, "node", 120, "growpart /dev/sda 1", false, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            (result.exitcode, result.out.as_str(), result.err.as_str()),
            (exit, out, err)
        );
        let nested = QgaGuest::new(&pve, "node", 120)
            .run(&["growpart", "/dev/sda", "1"])
            .await
            .unwrap();
        assert_eq!(nested.is_ok(), exit == 0);
        if exit == 1 {
            assert!(nested.unwrap_err().contains(if err.is_empty() { out } else { err }));
        }
    }
}
