//! The client's choice of instance (`docs/migration/phase-11.md`, 2.4) on
//! stand-ins for instances: reads go to the first that answers; a plan and
//! its job stay on one; a lost instance is reported, not replaced.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post};
use hs_client::{Client, ClientConfig, ClientError, Endpoint};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error>;

const TOKEN: &str = "client-token-0123456789abcdef0123456789abcdef";

/// A stand-in instance named `name`; `down` makes it answer like Caddy in
/// front of a stopped server (502, no error document).
async fn instance(name: &'static str, down: Arc<AtomicBool>) -> Result<Endpoint, Error> {
    let gate = move |down: Arc<AtomicBool>| move || !down.load(Ordering::SeqCst);
    let up = gate(down);
    let up_status = up.clone();
    let up_plan = up.clone();
    let up_apply = up.clone();
    let up_job = up;
    let app = Router::new()
        .route(
            "/v1/status",
            get(move || async move {
                if up_status() {
                    (StatusCode::OK, Json(json!({"instance": name})))
                } else {
                    (StatusCode::BAD_GATEWAY, Json(Value::Null))
                }
            }),
        )
        .route(
            "/v1/workspaces/{target}",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"schema_version": 1, "error": {"code": "not_found", "message": "No such workspace."}})),
                )
            }),
        )
        .route(
            "/v1/plans",
            post(move || async move {
                if up_plan() {
                    (StatusCode::CREATED, Json(json!({"id": name, "digest": "d"})))
                } else {
                    (StatusCode::BAD_GATEWAY, Json(Value::Null))
                }
            }),
        )
        .route(
            "/v1/plans/{id}/apply",
            post(move || async move {
                if up_apply() {
                    (
                        StatusCode::ACCEPTED,
                        Json(json!({"id": format!("job-{name}"), "state": "queued"})),
                    )
                } else {
                    (StatusCode::BAD_GATEWAY, Json(Value::Null))
                }
            }),
        )
        .route(
            "/v1/jobs/{id}",
            get(move || async move {
                if up_job() {
                    (
                        StatusCode::OK,
                        Json(json!({"id": format!("job-{name}"), "state": "succeeded"})),
                    )
                } else {
                    (StatusCode::BAD_GATEWAY, Json(Value::Null))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(Endpoint::from(format!("http://{addr}").parse::<url::Url>()?))
}

fn closed_port() -> Result<Endpoint, Error> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    Ok(Endpoint::from(format!("http://127.0.0.1:{port}").parse::<url::Url>()?))
}

fn client(endpoints: Vec<Endpoint>) -> Result<Client, Error> {
    let mut config = ClientConfig::new(endpoints, TOKEN.to_owned());
    config.connect_timeout = Duration::from_millis(500);
    Ok(Client::new(config)?)
}

#[tokio::test]
async fn reads_go_to_the_first_instance_that_answers() -> Result<(), Error> {
    let a_down = Arc::new(AtomicBool::new(false));
    let a = instance("a", Arc::clone(&a_down)).await?;
    let b = instance("b", Arc::new(AtomicBool::new(false))).await?;
    let client = client(vec![closed_port()?, a, b])?;

    assert_eq!(client.get("/v1/status").await?["instance"], "a");
    // A's server stops; Caddy still answers for it.
    a_down.store(true, Ordering::SeqCst);
    assert_eq!(client.get("/v1/status").await?["instance"], "b");
    // An instance's own answer is final, even an error.
    let Err(error) = client.get("/v1/workspaces/nope").await else {
        return Err("a 404 must not be retried elsewhere".into());
    };
    assert_eq!(error.code(), Some("not_found"));
    Ok(())
}

#[tokio::test]
async fn no_instance_answering_is_reported() -> Result<(), Error> {
    let client = client(vec![closed_port()?, closed_port()?])?;
    assert!(matches!(
        client.get("/v1/status").await,
        Err(ClientError::Unreachable(_))
    ));
    Ok(())
}

#[tokio::test]
async fn a_plan_and_its_job_stay_on_one_instance() -> Result<(), Error> {
    let a_down = Arc::new(AtomicBool::new(false));
    let a = instance("a", Arc::clone(&a_down)).await?;
    let b = instance("b", Arc::new(AtomicBool::new(false))).await?;
    let client = client(vec![a, b])?;

    let session = client.plan("workspace.refresh", json!({"vmid": 240})).await?;
    assert_eq!(session.plan["id"], "a");
    let job = session.apply("version").await?;
    assert_eq!(job["id"], "job-a");
    let done = session.wait(&job, Duration::from_secs(5)).await?;
    assert_eq!(done["state"], "succeeded");

    // The instance with the job goes away: the client says so and does
    // not ask B, which knows nothing of the job.
    a_down.store(true, Ordering::SeqCst);
    let Err(ClientError::Lost { url, .. }) = session.job("job-a").await else {
        return Err("a lost instance must be reported".into());
    };
    assert_eq!(&url, session.url().as_str());
    // A new plan goes to B.
    assert_eq!(
        client.plan("workspace.refresh", json!({"vmid": 240})).await?.plan["id"],
        "b"
    );
    Ok(())
}
