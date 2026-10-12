//! A Proxmox stand-in for the ports of the Python unit tests: it answers
//! from a fixed table and panics on any other request, as the Python tests'
//! fake sessions raise `AssertionError`.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use hs_core::pve::Pve;
use hs_model::config::Config;
use hs_pve::PveError;
use serde_json::Value;

/// What the fake answers to one request.
#[derive(Clone)]
enum Answer {
    Data(Value),
    Api(u16, String),
    /// The request never reached Proxmox (a transport failure).
    Transport(String),
    /// No answer ever arrives.
    Hang,
}

pub struct FakePve {
    /// Answers per request, used in order; the last one repeats.
    responses: Mutex<BTreeMap<String, VecDeque<Answer>>>,
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

/// `POST /path` or `POST /path?key=value&...` (form fields in the order sent).
pub fn form_key(method: &str, path: &str, form: &[(&str, &str)]) -> String {
    let form: Vec<String> = form.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if form.is_empty() {
        format!("{method} {path}")
    } else {
        format!("{method} {path}?{}", form.join("&"))
    }
}

/// The request key of `agent/exec` running `/bin/sh -lc COMMAND`.
pub fn exec_key(node: &str, vmid: u32, command: &str) -> String {
    format!("POST /nodes/{node}/qemu/{vmid}/agent/exec?command=/bin/sh&command=-lc&command={command}")
}

impl FakePve {
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn push(self, request: &str, answer: Answer) -> Self {
        if let Ok(mut responses) = self.responses.lock() {
            responses.entry(request.to_owned()).or_default().push_back(answer);
        }
        self
    }

    /// Answer `request` (`GET /path?query`) with `data`. Several answers to
    /// one request are given in order; the last one repeats.
    #[must_use]
    pub fn with(self, request: &str, data: Value) -> Self {
        self.push(request, Answer::Data(data))
    }

    /// Fail `request` with an HTTP status and message.
    #[must_use]
    pub fn failing(self, request: &str, status: u16, message: &str) -> Self {
        self.push(request, Answer::Api(status, message.to_owned()))
    }

    /// Fail `request` before it reaches Proxmox.
    #[must_use]
    pub fn unreachable(self, request: &str, detail: &str) -> Self {
        self.push(request, Answer::Transport(detail.to_owned()))
    }

    /// Never answer `request`.
    #[must_use]
    pub fn hanging(self, request: &str) -> Self {
        self.push(request, Answer::Hang)
    }

    /// `exec` of `command` gets `pid`; its `exec-status` answers `status`.
    #[must_use]
    pub fn with_exec(self, node: &str, vmid: u32, command: &str, pid: u32, status: Value) -> Self {
        self.with(&exec_key(node, vmid, command), serde_json::json!({ "pid": pid }))
            .with(
                &key(
                    "GET",
                    &format!("/nodes/{node}/qemu/{vmid}/agent/exec-status"),
                    &[("pid", &pid.to_string())],
                ),
                status,
            )
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|calls| calls.clone()).unwrap_or_default()
    }

    async fn answer(&self, request: String) -> Result<Value, PveError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(request.clone());
        }
        let answer = self.responses.lock().ok().and_then(|mut responses| {
            let queue = responses.get_mut(&request)?;
            if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().cloned()
            }
        });
        match answer {
            Some(Answer::Data(data)) => Ok(data),
            // As `PveClient` maps the HTTP status.
            Some(Answer::Api(401, _)) => Err(PveError::Unauthorized),
            Some(Answer::Api(403, message)) => Err(PveError::Forbidden { message }),
            Some(Answer::Api(595, message)) => Err(PveError::NodeUnreachable { message }),
            Some(Answer::Api(status, message)) => Err(PveError::Api { status, message }),
            Some(Answer::Transport(detail)) => Err(PveError::Response(detail)),
            Some(Answer::Hang) => std::future::pending().await,
            None => panic!("unexpected request: {request}"),
        }
    }
}

impl Pve for FakePve {
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.answer(key("GET", path, query)).await
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.answer(form_key("POST", path, form)).await
    }

    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.answer(form_key("PUT", path, form)).await
    }

    async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.answer(key("DELETE", path, query)).await
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
