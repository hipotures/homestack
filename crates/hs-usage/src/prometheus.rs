//! Bounded, shared, singleflight Prometheus execution and provenance cache.
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

const QUERY_CONCURRENCY: usize = 4;
const MAX_RESPONSE: usize = 1024 * 1024;
const CACHE_BYTES: usize = 12 * 1024 * 1024;
const CACHE_ENTRIES: usize = 96;
#[derive(Debug)]
pub struct Source {
    pub query: String,
    pub params: Vec<(String, String)>,
    pub data: Value,
    pub fetched: u64,
    pub bytes: usize,
}
type CachedResult = Option<(Instant, Result<Arc<Source>, String>)>;
struct Entry {
    value: AsyncMutex<CachedResult>,
    touched: Mutex<Instant>,
    bytes: AtomicUsize,
}
pub struct Prometheus {
    client: reqwest::Client,
    url: url::Url,
    slots: Semaphore,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
impl Prometheus {
    pub fn new(client: reqwest::Client, url: url::Url) -> Self {
        Self {
            client,
            url,
            slots: Semaphore::new(QUERY_CONCURRENCY),
            entries: Mutex::new(HashMap::new()),
        }
    }
    pub async fn read(&self, path: &str, params: Vec<(String, String)>, ttl: u64) -> Result<Arc<Source>, String> {
        let key = format!("{path}{params:?}");
        let entry = {
            let mut entries = self.entries.lock().map_err(|_| "query cache poisoned")?;
            if !entries.contains_key(&key) && entries.len() >= CACHE_ENTRIES {
                evict(&mut entries, None)?;
            }
            let entry = Arc::clone(entries.entry(key.clone()).or_insert_with(|| {
                Arc::new(Entry {
                    value: AsyncMutex::new(None),
                    touched: Mutex::new(Instant::now()),
                    bytes: AtomicUsize::new(0),
                })
            }));
            if let Ok(mut touched) = entry.touched.lock() {
                *touched = Instant::now();
            }
            entry
        };
        // Cancellation releases the lock. Another waiter resumes the miss; failed reads coalesce for 5s.
        let mut value = entry.value.lock().await;
        if let Some((at, source)) = value.as_ref()
            && at.elapsed() < Duration::from_secs(if source.is_ok() { ttl } else { 5 })
        {
            return source.clone();
        }
        let result = self.execute(path, params).await;
        if let Ok(source) = &result {
            let mut entries = self.entries.lock().map_err(|_| "query cache poisoned")?;
            loop {
                let bytes: usize = entries
                    .iter()
                    .filter(|(k, _)| *k != &key)
                    .map(|(_, e)| e.bytes.load(Ordering::Relaxed))
                    .sum();
                if bytes + source.bytes <= CACHE_BYTES {
                    entry.bytes.store(source.bytes, Ordering::Relaxed);
                    break;
                }
                evict(&mut entries, Some(&key))?;
            }
        }
        if result.is_err() {
            entry.bytes.store(0, Ordering::Relaxed);
        }
        *value = Some((Instant::now(), result.clone()));
        result
    }
    async fn execute(&self, path: &str, params: Vec<(String, String)>) -> Result<Arc<Source>, String> {
        let _permit = self.slots.acquire().await.map_err(|_| "query scheduler closed")?;
        let started = Instant::now();
        let mut url = self.url.clone();
        url.set_path(path);
        url.query_pairs_mut().extend_pairs(&params);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?
            .error_for_status()
            .map_err(|e| e.without_url().to_string())?;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.without_url().to_string())?;
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err("Prometheus result exceeded the 1 MiB safety limit; this view is unavailable at its current cardinality".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if body["status"] != "success" {
            return Err("Prometheus query failed or exceeded its time limit".into());
        }
        if body["warnings"].as_array().is_some_and(|a| !a.is_empty()) {
            return Err("Prometheus returned warnings; refusing an incomplete result".into());
        }
        let source = Arc::new(Source {
            query: params
                .iter()
                .find(|(k, _)| k == "query")
                .map_or_else(|| path.to_owned(), |(_, v)| v.clone()),
            params,
            data: body["data"].clone(),
            fetched: now(),
            bytes: bytes.len() * 4,
        });
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis(),
            bytes = bytes.len(),
            "Prometheus query"
        );
        Ok(source)
    }
    pub async fn instant(&self, query: String, time: f64, ttl: u64) -> Result<Arc<Source>, String> {
        self.read(
            "/api/v1/query",
            vec![
                ("query".into(), query),
                ("time".into(), time.to_string()),
                ("timeout".into(), "20s".into()),
            ],
            ttl,
        )
        .await
    }
}
fn evict(entries: &mut HashMap<String, Arc<Entry>>, except: Option<&String>) -> Result<(), String> {
    let oldest = entries
        .iter()
        .filter(|(k, v)| Some(*k) != except && Arc::strong_count(v) == 1)
        .min_by_key(|(_, v)| v.touched.lock().map_or_else(|_| Instant::now(), |t| *t))
        .map(|(k, _)| k.clone());
    if let Some(key) = oldest {
        entries.remove(&key);
        Ok(())
    } else {
        Err("Query cache is busy; retry shortly".into())
    }
}
