//! Proxmox reads fetched several at a time ahead of code that reads them one
//! by one: `warm` fills a cache, `get` answers from it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use hs_pve::PveError;
use serde_json::Value;

use crate::pve::Pve;

/// How long a guest agent may take; Proxmox itself waits 3 s for a silent one.
const AGENT_TIMEOUT: Duration = Duration::from_secs(1);
const LABEL_REFRESH: Duration = Duration::from_secs(60);
const STATISTICS_MAX_AGE: Duration = Duration::from_secs(30);
const LABEL_MAX_AGE: Duration = Duration::from_secs(120);

type Key = (String, Vec<(String, String)>);

/// One read of [`Prefetch::warm`] and its answer.
type Read<'a> = Pin<Box<dyn Future<Output = Vec<(Key, Result<Value, PveError>)>> + Send + 'a>>;

struct Observation {
    measured: Instant,
    value: Value,
}

/// Recent guest observations used only by status reporting, never disk operations.
#[derive(Default)]
pub struct Observations {
    reads: Mutex<HashMap<Key, Observation>>,
    configs: Mutex<HashMap<String, Value>>,
}

fn guest(path: &str) -> Option<&str> {
    path.split_once("/agent/").map(|(vm, _)| vm)
}

fn is_label(key: &Key) -> bool {
    key.0.ends_with("/agent/file-read")
        && key.1.iter().any(|(name, value)| name == "offset" && value == "1024")
        && key.1.iter().any(|(name, value)| name == "count" && value == "256")
}

impl Observations {
    fn recent(&self, key: &Key, age: Duration) -> Option<Value> {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .filter(|read| read.measured.elapsed() < age)
            .map(|read| read.value.clone())
    }

    fn config(&self, path: &str, value: &Value) {
        let Some(vm) = path.strip_suffix("/config").filter(|vm| vm.contains("/qemu/")) else {
            return;
        };
        let changed = self
            .configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.to_owned(), value.clone())
            .is_some_and(|previous| previous != *value);
        if changed {
            self.reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|key, _| guest(&key.0) != Some(vm));
        }
    }
}

/// A [`Pve`] whose reads come from a cache filled `parallel` at a time.
pub struct Prefetch<'a, P> {
    pve: &'a P,
    parallel: usize,
    cache: Mutex<HashMap<Key, Result<Value, String>>>,
    observations: &'a Observations,
    stale: Mutex<HashSet<String>>,
}

fn key(path: &str, query: &[(&str, &str)]) -> Key {
    (
        path.to_owned(),
        query.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
    )
}

impl<'a, P: Pve> Prefetch<'a, P> {
    #[must_use]
    pub fn new(pve: &'a P, parallel: usize, observations: &'a Observations) -> Self {
        Self {
            pve,
            parallel: parallel.max(1),
            cache: Mutex::new(HashMap::new()),
            observations,
            stale: Mutex::default(),
        }
    }

    /// Drop observations when a VM stops, disappears or moves to another node.
    pub fn retain_guests(&self, running: &HashSet<String>) {
        self.observations
            .reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|key, _| guest(&key.0).is_some_and(|vm| running.contains(vm)));
        self.observations
            .configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|vm, _| running.contains(vm));
    }

    pub fn used_previous_guest_data(&self, vm: &str) -> bool {
        self.stale
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(vm)
    }

    fn observe(&self, key: &Key, result: Result<Value, PveError>) -> Result<Value, PveError> {
        let age = if is_label(key) {
            LABEL_MAX_AGE
        } else if key.0.ends_with("/agent/get-fsinfo") {
            STATISTICS_MAX_AGE
        } else {
            return result;
        };
        match result {
            Ok(value) => {
                self.observations
                    .reads
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        key.clone(),
                        Observation {
                            measured: Instant::now(),
                            value: value.clone(),
                        },
                    );
                Ok(value)
            }
            Err(error) => {
                if !matches!(error, PveError::Unauthorized | PveError::Forbidden { .. })
                    && let Some(value) = self.observations.recent(key, age)
                {
                    if let Some(vm) = guest(&key.0) {
                        self.stale
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(vm.to_owned());
                    }
                    return Ok(value);
                }
                Err(error)
            }
        }
    }

    /// Read every `(path, query)` not read yet, `parallel` at a time.
    pub async fn warm(&self, requests: Vec<(String, Vec<(&str, &str)>)>) {
        let missing: Vec<_> = {
            let cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            requests
                .into_iter()
                .filter(|(path, query)| !cache.contains_key(&key(path, query)))
                .collect()
        };
        let mut groups: HashMap<String, Vec<Key>> = HashMap::new();
        for (path, query) in missing {
            let key = key(&path, &query);
            if is_label(&key)
                && let Some(value) = self.observations.recent(&key, LABEL_REFRESH)
            {
                self.cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key, Ok(value));
                continue;
            }
            groups
                .entry(guest(&path).unwrap_or(&path).to_owned())
                .or_default()
                .push(key);
        }
        // Each agent serves one request at a time; different VMs remain parallel.
        let reads: Vec<Read<'_>> = groups
            .into_values()
            .map(|keys| {
                Box::pin(async move {
                    let mut results = Vec::new();
                    let deadline = tokio::time::Instant::now() + AGENT_TIMEOUT;
                    for key in keys {
                        if guest(&key.0).is_some() && tokio::time::Instant::now() >= deadline {
                            results.push((
                                key,
                                Err(PveError::Response("the guest agent did not answer".to_owned())),
                            ));
                            continue;
                        }
                        let query: Vec<(&str, &str)> = key.1.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                        let read = self.pve.get(&key.0, &query);
                        let result = if guest(&key.0).is_some() {
                            match tokio::time::timeout_at(deadline, read).await {
                                Ok(result) => result,
                                Err(_) => Err(PveError::Response("the guest agent did not answer".to_owned())),
                            }
                        } else {
                            read.await
                        };
                        results.push((key, result));
                    }
                    results
                }) as Read<'_>
            })
            .collect();
        let results: Vec<Vec<(Key, Result<Value, PveError>)>> = futures_util::stream::iter(reads)
            .buffer_unordered(self.parallel)
            .collect()
            .await;
        for (key, result) in results.iter().flatten() {
            if let Ok(value) = result {
                self.observations.config(&key.0, value);
            }
        }
        let results: Vec<_> = results
            .into_iter()
            .flatten()
            .map(|(key, result)| {
                let result = self.observe(&key, result).map_err(|error| error.to_string());
                (key, result)
            })
            .collect();
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(results);
    }
}

impl<P: Pve> Pve for Prefetch<'_, P> {
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key(path, query))
            .cloned();
        match cached {
            Some(result) => result.map_err(PveError::Response),
            None => {
                let key = key(path, query);
                let result = self.observe(&key, self.pve.get(path, query).await);
                if let Ok(value) = &result {
                    self.cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key, Ok(value.clone()));
                }
                result
            }
        }
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.pve.post(path, form).await
    }

    async fn put(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, PveError> {
        self.pve.put(path, form).await
    }

    async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, PveError> {
        self.pve.delete(path, query).await
    }
}
