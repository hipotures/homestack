//! One cancellable detail read for the current UI demand.

use std::sync::Arc;
use std::time::Duration;

use hs_client::Client;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::app::client_error;

const DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Default)]
pub(super) struct Loader {
    target: Option<(u32, bool)>,
    task: Option<JoinHandle<Result<Value, String>>>,
}

impl Loader {
    /// Replacing or closing the demand also drops any completed, obsolete result.
    pub fn update(&mut self, target: Option<(u32, bool)>, client: Option<&Arc<Client>>) {
        let target = target.filter(|_| client.is_some());
        if self.target.map(|(vmid, _)| vmid) == target.map(|(vmid, _)| vmid) {
            return;
        }
        self.cancel();
        if let (Some((vmid, debounce)), Some(client)) = (target, client) {
            let client = Arc::clone(client);
            self.target = target;
            self.task = Some(tokio::spawn(async move {
                if debounce {
                    tokio::time::sleep(DEBOUNCE).await;
                }
                client
                    .get(&format!("/v1/workspaces/{vmid}"))
                    .await
                    .map_err(|error| client_error(error).0)
            }));
        }
    }

    /// Safe to select against input: the handle stays here until it completes.
    pub async fn receive(&mut self) -> (u32, Result<Value, String>) {
        let (Some((vmid, _)), Some(task)) = (self.target, self.task.as_mut()) else {
            return std::future::pending().await;
        };
        let result = task
            .await
            .unwrap_or_else(|error| Err(format!("Cannot read VM details: {error}")));
        self.task = None;
        self.target = None;
        (vmid, result)
    }

    pub fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.target = None;
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::StatusCode,
        routing::get,
    };
    use hs_client::ClientConfig;
    use serde_json::json;
    use tokio::sync::{Notify, mpsc};
    use tokio::time::{advance, timeout};

    struct Server {
        client: Arc<Client>,
        requests: mpsc::UnboundedReceiver<u32>,
        release: Arc<Notify>,
        task: JoinHandle<()>,
    }

    impl Server {
        async fn start() -> Self {
            let (send, requests) = mpsc::unbounded_channel();
            let release = Arc::new(Notify::new());
            let app = Router::new().route("/v1/workspaces/{vmid}", get(
                |Path(vmid): Path<u32>, State((send, release)): State<(mpsc::UnboundedSender<u32>, Arc<Notify>)>| async move {
                    send.send(vmid).unwrap();
                    if vmid == 201 {
                        release.notified().await;
                        (StatusCode::CONFLICT, Json(json!({"error": {"code": "outside_pool", "message": "old failure"}})))
                    } else {
                        (StatusCode::OK, Json(json!({"vmid": vmid})))
                    }
                }
            )).with_state((send, Arc::clone(&release)));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let client = Arc::new(
                Client::new(ClientConfig::new(
                    vec![url.parse::<url::Url>().unwrap().into()],
                    "test-token".to_owned(),
                ))
                .unwrap(),
            );
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                client,
                requests,
                release,
                task,
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rapid_navigation_reads_only_the_last_vm_after_150_ms() {
        let mut server = Server::start().await;
        let mut loader = Loader::default();
        for vmid in [201, 202, 203] {
            loader.update(Some((vmid, true)), Some(&server.client));
            tokio::task::yield_now().await;
            advance(Duration::from_millis(100)).await;
        }
        advance(Duration::from_millis(49)).await;
        tokio::task::yield_now().await;
        assert!(server.requests.try_recv().is_err());
        advance(Duration::from_millis(1)).await;
        tokio::time::resume();
        let (vmid, result) = timeout(Duration::from_secs(2), loader.receive()).await.unwrap();
        assert_eq!(vmid, 203);
        assert_eq!(result.unwrap()["vmid"], 203);
        assert_eq!(server.requests.try_recv().unwrap(), 203);
        assert!(server.requests.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn closing_info_cancels_the_debounced_read() {
        let mut server = Server::start().await;
        let mut loader = Loader::default();
        loader.update(Some((202, true)), Some(&server.client));
        tokio::task::yield_now().await;
        loader.update(None, Some(&server.client));
        advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(server.requests.try_recv().is_err());
        assert!(timeout(Duration::from_secs(1), loader.receive()).await.is_err());
    }

    #[tokio::test]
    async fn changing_selection_discards_an_in_flight_error() {
        let mut server = Server::start().await;
        let mut loader = Loader::default();
        loader.update(Some((201, false)), Some(&server.client));
        assert_eq!(
            timeout(Duration::from_secs(2), server.requests.recv()).await.unwrap(),
            Some(201)
        );
        loader.update(Some((202, false)), Some(&server.client));
        server.release.notify_one();
        let (vmid, result) = timeout(Duration::from_secs(2), loader.receive()).await.unwrap();
        assert_eq!(vmid, 202);
        assert_eq!(result.unwrap()["vmid"], 202);
        assert!(timeout(Duration::from_millis(20), loader.receive()).await.is_err());
    }

    #[tokio::test]
    async fn repeated_demand_reuses_the_in_flight_read() {
        let mut server = Server::start().await;
        let mut loader = Loader::default();
        loader.update(Some((201, false)), Some(&server.client));
        assert_eq!(
            timeout(Duration::from_secs(2), server.requests.recv()).await.unwrap(),
            Some(201)
        );
        loader.update(Some((201, false)), Some(&server.client));
        loader.update(Some((201, true)), Some(&server.client));
        server.release.notify_one();
        let (vmid, result) = timeout(Duration::from_secs(2), loader.receive()).await.unwrap();
        assert_eq!(vmid, 201);
        assert!(result.unwrap_err().contains("old failure"));
        assert!(server.requests.try_recv().is_err());
    }
}
