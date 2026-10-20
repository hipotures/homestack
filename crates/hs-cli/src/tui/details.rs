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
