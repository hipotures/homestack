//! Proxmox reads fetched several at a time ahead of code that reads them one
//! by one: `warm` fills a cache, `get` answers from it.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use hs_pve::PveError;
use serde_json::Value;

use crate::pve::Pve;

/// How long a guest agent may take; Proxmox itself waits 3 s for a silent one.
const AGENT_TIMEOUT: Duration = Duration::from_secs(1);

type Key = (String, Vec<(String, String)>);

/// One read of [`Prefetch::warm`] and its answer.
type Read<'a> = Pin<Box<dyn Future<Output = (Key, Result<Value, String>)> + Send + 'a>>;

/// A [`Pve`] whose reads come from a cache filled `parallel` at a time.
pub struct Prefetch<'a, P> {
    pve: &'a P,
    parallel: usize,
    cache: Mutex<HashMap<Key, Result<Value, String>>>,
}

fn key(path: &str, query: &[(&str, &str)]) -> Key {
    (
        path.to_owned(),
        query.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
    )
}

impl<'a, P: Pve> Prefetch<'a, P> {
    #[must_use]
    pub fn new(pve: &'a P, parallel: usize) -> Self {
        Self {
            pve,
            parallel: parallel.max(1),
            cache: Mutex::new(HashMap::new()),
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
        // Boxed as `Send` one by one: rustc cannot prove it through the
        // stream adapters' closures.
        let reads: Vec<Read<'_>> = missing
            .into_iter()
            .map(|(path, query)| {
                let key = key(&path, &query);
                Box::pin(async move {
                    let query: Vec<(&str, &str)> = key.1.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    let read = self.pve.get(&key.0, &query);
                    let result = if path.contains("/agent/") {
                        match tokio::time::timeout(AGENT_TIMEOUT, read).await {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(_) => Err("the guest agent did not answer".to_owned()),
                        }
                    } else {
                        read.await.map_err(|error| error.to_string())
                    };
                    (key, result)
                }) as Read<'_>
            })
            .collect();
        let results: Vec<(Key, Result<Value, String>)> = futures_util::stream::iter(reads)
            .buffer_unordered(self.parallel)
            .collect()
            .await;
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
                let result = self.pve.get(path, query).await;
                if let Ok(value) = &result {
                    self.cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key(path, query), Ok(value.clone()));
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
