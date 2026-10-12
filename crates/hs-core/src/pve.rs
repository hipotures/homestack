//! The Proxmox API as the operations see it: JSON in, JSON out.

use std::future::Future;

use hs_pve::{PveClient, PveError};
use serde_json::Value;

/// Reads and calls on `/api2/json`. Paths start with `/`; the answer is
/// Proxmox's `data`. `POST` and `PUT` send form fields, in order (a repeated
/// name is an array, e.g. `command` of `agent/exec`).
pub trait Pve: Sync {
    fn get(&self, path: &str, query: &[(&str, &str)]) -> impl Future<Output = Result<Value, PveError>> + Send;
    fn post(&self, path: &str, form: &[(&str, &str)]) -> impl Future<Output = Result<Value, PveError>> + Send;
    fn put(&self, path: &str, form: &[(&str, &str)]) -> impl Future<Output = Result<Value, PveError>> + Send;
    fn delete(&self, path: &str, query: &[(&str, &str)]) -> impl Future<Output = Result<Value, PveError>> + Send;
}

fn segments(path: &str) -> Vec<&str> {
    path.trim_start_matches('/').split('/').collect()
}

impl Pve for PveClient {
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.get_value(&segments(path), query).await
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.post_value(&segments(path), form).await
    }

    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.put_value(&segments(path), form).await
    }

    async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.delete_value(&segments(path), query).await
    }
}
