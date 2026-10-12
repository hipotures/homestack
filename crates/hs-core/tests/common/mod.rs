//! A Proxmox stand-in for the ports of the Python unit tests: it answers
//! from a fixed table and panics on any other request, as the Python tests'
//! fake sessions raise `AssertionError`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use hs_core::pve::Pve;
use hs_model::config::Config;
use hs_pve::PveError;
use serde_json::Value;

pub struct FakePve {
    responses: BTreeMap<String, Result<Value, (u16, String)>>,
    calls: Mutex<Vec<String>>,
}

/// `GET /path` or `GET /path?key=value&...` (query sorted).
pub fn key(method: &str, path: &str, query: &[(&str, &str)]) -> String {
    let mut query: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    query.sort();
    if query.is_empty() {
        format!("{method} {path}")
    } else {
        format!("{method} {path}?{}", query.join("&"))
    }
}

impl FakePve {
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Answer `request` (`GET /path?query`) with `data`.
    #[must_use]
    pub fn with(mut self, request: &str, data: Value) -> Self {
        self.responses.insert(request.to_owned(), Ok(data));
        self
    }

    /// Fail `request` with an HTTP status and message.
    #[must_use]
    pub fn failing(mut self, request: &str, status: u16, message: &str) -> Self {
        self.responses
            .insert(request.to_owned(), Err((status, message.to_owned())));
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|calls| calls.clone()).unwrap_or_default()
    }

    fn answer(&self, request: String) -> Result<Value, PveError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(request.clone());
        }
        match self.responses.get(&request) {
            Some(Ok(data)) => Ok(data.clone()),
            Some(Err((status, message))) => Err(PveError::Api {
                status: *status,
                message: message.clone(),
            }),
            None => panic!("unexpected request: {request}"),
        }
    }
}

impl Pve for FakePve {
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.answer(key("GET", path, query))
    }

    async fn post(&self, path: &str) -> Result<Value, PveError> {
        self.answer(key("POST", path, &[]))
    }
}

/// The configuration of Python's `tests/support.test_config()`.
pub fn test_config() -> Config {
    let text = r#"version = 1
[node]
name = "example-node-1"
gold_vmid = 101
[storage_layouts]
homestack-storage-1 = ["example-storage-a"]
homestack-storage-2 = ["example-storage-b"]
homestack-storage-3 = ["example-storage-b"]
[root]
disk = "scsi0"
[home]
disk = "scsi1"
default_size = "20G"
[network]
prefix = "192.0.2"
cidr = 24
gateway = "192.0.2.1"
dns = ["192.0.2.1", "1.1.1.1"]
[cloud_init]
snippet_storage = "local"
snippet_dir = "/var/lib/vz/snippets"
[user]
name = "user"
uid = 1000
gid = 1000
"#;
    match Config::parse(text) {
        Ok(config) => config,
        Err(error) => panic!("test config: {error}"),
    }
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(future),
        Err(error) => panic!("runtime: {error}"),
    }
}
