//! OTLP/HTTP JSON metrics. Cumulative counters retain reset boundaries;
//! cached filesystem samples retain their observation time and are sent once.
#![allow(clippy::cast_precision_loss)]

use crate::config::Config;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::time::{Duration, Instant};

type Attributes = BTreeMap<String, String>;

struct Counter {
    start: u64,
    previous: Value,
    generation: String,
}

#[derive(Default)]
struct Encoder {
    counters: BTreeMap<String, Counter>,
    filesystem_samples: BTreeMap<String, u64>,
}

struct Batch {
    payload: Value,
    points: usize,
    filesystem_samples: BTreeMap<String, u64>,
}

struct Metrics<'a> {
    encoder: &'a mut Encoder,
    metrics: BTreeMap<String, Value>,
    seen_counters: BTreeSet<String>,
    points: usize,
}

fn attributes(values: &Attributes) -> Vec<Value> {
    values
        .iter()
        .map(|(key, value)| json!({"key": key, "value": {"stringValue": value}}))
        .collect()
}

fn with(values: &Attributes, key: &str, value: &str) -> Attributes {
    let mut result = values.clone();
    result.insert(key.to_owned(), value.to_owned());
    result
}

impl Metrics<'_> {
    fn point(
        &mut self,
        name: &str,
        unit: &str,
        (raw, scale): (&Value, f64),
        labels: &Attributes,
        time: u64,
        generation: Option<&str>,
    ) {
        let Some(number) = raw.as_f64().filter(|n| n.is_finite()) else {
            return;
        };
        let mut point = json!({"attributes": attributes(labels), "timeUnixNano": time.to_string()});
        if scale == 1.0 && raw.as_u64().is_some_and(|n| i64::try_from(n).is_ok()) {
            point["asInt"] = json!(raw.to_string());
        } else {
            point["asDouble"] = json!(number * scale);
        }
        let is_counter = generation.is_some();
        if let Some(generation) = generation {
            let key = format!("{name}:{}", json!(labels));
            let counter = self.encoder.counters.entry(key.clone()).or_insert_with(|| Counter {
                start: time,
                previous: raw.clone(),
                generation: generation.to_owned(),
            });
            let decreased = match (raw.as_u64(), counter.previous.as_u64()) {
                (Some(current), Some(previous)) => current < previous,
                _ => number < counter.previous.as_f64().unwrap_or(number),
            };
            if counter.generation != generation || decreased {
                counter.start = time;
                counter.generation = generation.to_owned();
            }
            counter.previous = raw.clone();
            point["startTimeUnixNano"] = json!(counter.start.to_string());
            self.seen_counters.insert(key);
        }
        let metric = self.metrics.entry(name.to_owned()).or_insert_with(|| {
            if is_counter {
                json!({"name": name, "unit": unit,
                    "sum": {"aggregationTemporality": 2, "isMonotonic": true, "dataPoints": []}})
            } else {
                json!({"name": name, "unit": unit, "gauge": {"dataPoints": []}})
            }
        });
        let kind = if is_counter { "sum" } else { "gauge" };
        if let Some(points) = metric[kind]["dataPoints"].as_array_mut() {
            points.push(point);
        }
        self.points += 1;
    }

    fn gauge(&mut self, name: &str, unit: &str, raw: &Value, labels: &Attributes, time: u64) {
        self.point(name, unit, (raw, 1.0), labels, time, None);
    }

    fn cgroup(&mut self, instance: &Value, labels: &Attributes, time: u64) {
        let effective = &instance["effective_limits"];
        self.gauge(
            "hs_pve_cpu_capacity",
            "{cpu}",
            &effective["cpu_capacity_cpus"],
            labels,
            time,
        );
        let memory = &effective["memory_limit_bytes"];
        if memory.is_number() || memory == "max" {
            self.gauge(
                "hs_pve_memory_effective_limit_unlimited",
                "1",
                &json!(u8::from(memory == "max")),
                labels,
                time,
            );
            self.gauge("hs_pve_memory_effective_limit_bytes", "By", memory, labels, time);
        }
        let metrics = &instance["metrics"];
        let generation = instance["cgroup_identity"].as_str().unwrap_or_default();
        for (field, name) in [
            ("usage_usec", "usage"),
            ("user_usec", "user"),
            ("system_usec", "system"),
            ("nice_usec", "nice"),
            ("throttled_usec", "throttled"),
            ("burst_usec", "burst"),
            ("core_sched.force_idle_usec", "force_idle"),
        ] {
            self.point(
                &format!("hs_pve_cpu_{name}_seconds"),
                "s",
                (&metrics["cpu.stat"][field], 1e-6),
                labels,
                time,
                Some(generation),
            );
        }
        for (field, name) in [
            ("nr_periods", "periods"),
            ("nr_throttled", "throttled_periods"),
            ("nr_bursts", "bursts"),
        ] {
            self.point(
                &format!("hs_pve_cpu_{name}"),
                "{event}",
                (&metrics["cpu.stat"][field], 1.0),
                labels,
                time,
                Some(generation),
            );
        }
        self.gauge("hs_pve_cpu_weight", "1", &metrics["cpu.weight"], labels, time);
        if let Some(cpus) = metrics["cpuset.cpus.effective"]
            .as_str()
            .and_then(crate::limits::cpu_count)
        {
            self.gauge("hs_pve_cpu_cpuset_count", "{cpu}", &json!(cpus), labels, time);
        }
        let quota = &metrics["cpu.max"]["quota_usec"];
        if quota.is_number() || quota == "max" {
            self.gauge(
                "hs_pve_cpu_quota_unlimited",
                "1",
                &json!(u8::from(quota == "max")),
                labels,
                time,
            );
            self.point("hs_pve_cpu_quota_seconds", "s", (quota, 1e-6), labels, time, None);
        }
        self.point(
            "hs_pve_cpu_quota_period_seconds",
            "s",
            (&metrics["cpu.max"]["period_usec"], 1e-6),
            labels,
            time,
            None,
        );
        for (file, name) in [
            ("memory.current", "usage"),
            ("memory.peak", "peak"),
            ("memory.swap.current", "swap_usage"),
        ] {
            self.gauge(
                &format!("hs_pve_memory_{name}_bytes"),
                "By",
                &metrics[file],
                labels,
                time,
            );
        }
        for (file, limit) in [
            ("memory.max", "hard"),
            ("memory.high", "high"),
            ("memory.swap.max", "swap"),
        ] {
            let value = &metrics[file];
            let labels = with(labels, "limit", limit);
            if value.is_number() || value == "max" {
                self.gauge(
                    "hs_pve_memory_limit_unlimited",
                    "1",
                    &json!(u8::from(value == "max")),
                    &labels,
                    time,
                );
                self.gauge("hs_pve_memory_limit_bytes", "By", value, &labels, time);
            }
        }
        for field in [
            "anon",
            "file",
            "kernel",
            "kernel_stack",
            "pagetables",
            "sec_pagetables",
            "percpu",
            "sock",
            "vmalloc",
            "shmem",
            "zswap",
            "zswapped",
            "file_mapped",
            "file_dirty",
            "file_writeback",
            "swapcached",
            "anon_thp",
            "file_thp",
            "shmem_thp",
            "inactive_anon",
            "active_anon",
            "inactive_file",
            "active_file",
            "unevictable",
            "slab_reclaimable",
            "slab_unreclaimable",
            "slab",
        ] {
            self.gauge(
                "hs_pve_memory_stat_bytes",
                "By",
                &metrics["memory.stat"][field],
                &with(labels, "kind", field),
                time,
            );
        }
        if let Some(fields) = metrics["memory.events"].as_object() {
            for (event, value) in fields {
                self.point(
                    "hs_pve_memory_events",
                    "{event}",
                    (value, 1.0),
                    &with(labels, "event", event),
                    time,
                    Some(generation),
                );
            }
        }
        if let Some(fields) = metrics["memory.stat"].as_object() {
            for (event, value) in fields {
                if ["pg", "thp_", "workingset_", "numa_", "pswp", "swp", "zswp"]
                    .iter()
                    .any(|prefix| event.starts_with(prefix))
                {
                    self.point(
                        "hs_pve_memory_stat_events",
                        "{event}",
                        (value, 1.0),
                        &with(labels, "event", event),
                        time,
                        Some(generation),
                    );
                }
            }
        }
        self.gauge(
            "hs_pve_pids_current",
            "{process}",
            &metrics["pids.current"],
            labels,
            time,
        );
        let pids_max = &metrics["pids.max"];
        if pids_max.is_number() || pids_max == "max" {
            self.gauge(
                "hs_pve_pids_limit_unlimited",
                "1",
                &json!(u8::from(pids_max == "max")),
                labels,
                time,
            );
            self.gauge("hs_pve_pids_limit", "{process}", pids_max, labels, time);
        }
        for field in ["populated", "frozen"] {
            self.gauge(
                &format!("hs_pve_cgroup_{field}"),
                "1",
                &metrics["cgroup.events"][field],
                labels,
                time,
            );
        }
        for resource in ["cpu", "memory", "io"] {
            for stall in ["some", "full"] {
                let labels = with(&with(labels, "resource", resource), "stall", stall);
                let psi = &metrics[format!("{resource}.pressure")][stall];
                for (field, window) in [("avg10", "10s"), ("avg60", "60s"), ("avg300", "300s")] {
                    self.point(
                        "hs_pve_pressure_stall_ratio",
                        "1",
                        (&psi[field], 0.01),
                        &with(&labels, "window", window),
                        time,
                        None,
                    );
                }
                self.point(
                    "hs_pve_pressure_stall_seconds",
                    "s",
                    (&psi["total"], 1e-6),
                    &labels,
                    time,
                    Some(generation),
                );
            }
        }
        if let Some(devices) = metrics["io.stat"].as_object() {
            for (device, fields) in devices {
                for (prefix, operation) in [("r", "read"), ("w", "write"), ("d", "discard")] {
                    let labels = with(&with(labels, "device", device), "operation", operation);
                    self.point(
                        "hs_pve_io_bytes",
                        "By",
                        (&fields[format!("{prefix}bytes")], 1.0),
                        &labels,
                        time,
                        Some(generation),
                    );
                    self.point(
                        "hs_pve_io_operations",
                        "{operation}",
                        (&fields[format!("{prefix}ios")], 1.0),
                        &labels,
                        time,
                        Some(generation),
                    );
                }
            }
        }
        self.point(
            "hs_pve_instance_collection_seconds",
            "s",
            (&instance["collection_usec"], 1e-6),
            labels,
            time,
            None,
        );
        self.gauge(
            "hs_pve_cgroup_read_errors",
            "{error}",
            &json!(instance["read_errors"].as_object().map_or(0, |e| e.len())),
            labels,
            time,
        );
    }

    fn filesystem(&mut self, instance: &Value, labels: &Attributes, time: u64, changed: bool) {
        let report = &instance["filesystem_report"];
        self.gauge(
            "hs_pve_filesystem_sample_age_seconds",
            "s",
            &report["sample_age_seconds"],
            labels,
            time,
        );
        if !changed {
            return;
        }
        let Some(sample_time) = report["sampled_at_unix_ms"]
            .as_u64()
            .and_then(|n| n.checked_mul(1_000_000))
        else {
            return;
        };
        self.gauge(
            "hs_pve_filesystem_collection_success",
            "1",
            &json!(u8::from(report["error"].is_null())),
            labels,
            sample_time,
        );
        let Some(filesystems) = report["filesystems"].as_array() else {
            return;
        };
        for filesystem in filesystems {
            let Some(mount) = filesystem["mount"].as_str() else {
                continue;
            };
            let mut labels = with(labels, "mount", mount);
            if let Some(kind) = filesystem["filesystem"].as_str() {
                labels.insert("filesystem".into(), kind.into());
            }
            self.gauge(
                "hs_pve_filesystem_mounted",
                "1",
                &json!(u8::from(filesystem["mounted"] == true)),
                &labels,
                sample_time,
            );
            self.gauge(
                "hs_pve_filesystem_read_success",
                "1",
                &json!(u8::from(filesystem["error"].is_null())),
                &labels,
                sample_time,
            );
            for field in [
                "total_bytes",
                "used_bytes",
                "free_bytes",
                "available_bytes",
                "total_bytes_unprivileged",
            ] {
                self.gauge(
                    &format!("hs_pve_filesystem_{field}"),
                    "By",
                    &filesystem[field],
                    &labels,
                    sample_time,
                );
            }
            for field in ["inodes_total", "inodes_free"] {
                self.gauge(
                    &format!("hs_pve_filesystem_{field}"),
                    "{inode}",
                    &filesystem[field],
                    &labels,
                    sample_time,
                );
            }
        }
    }
}

impl Encoder {
    fn encode(&mut self, report: &Value, node: &str, failures: u64, previous_success: bool) -> Result<Batch, String> {
        let time = report["unix_timestamp_ms"]
            .as_u64()
            .and_then(|t| t.checked_mul(1_000_000))
            .ok_or("invalid snapshot timestamp")?;
        let mut fs_samples = BTreeMap::new();
        let mut builder = Metrics {
            encoder: self,
            metrics: BTreeMap::new(),
            seen_counters: BTreeSet::new(),
            points: 0,
        };
        let node_labels = BTreeMap::from([("node".into(), node.into())]);
        builder.gauge(
            "hs_pve_agent_instances",
            "{instance}",
            &report["instance_count"],
            &node_labels,
            time,
        );
        builder.point(
            "hs_pve_agent_collection_seconds",
            "s",
            (&report["collection_usec"], 1e-6),
            &node_labels,
            time,
            None,
        );
        // The cycle length of the current mode, so readers judge freshness.
        builder.gauge(
            "hs_pve_agent_interval_seconds",
            "s",
            &report["interval_seconds"],
            &with(&node_labels, "mode", report["mode"].as_str().unwrap_or_default()),
            time,
        );
        builder.gauge(
            "hs_pve_agent_export_success",
            "1",
            &json!(u8::from(previous_success)),
            &node_labels,
            time,
        );
        builder.point(
            "hs_pve_agent_export_failures",
            "{event}",
            (&json!(failures), 1.0),
            &node_labels,
            time,
            Some("agent"),
        );
        if let Some(instances) = report["instances"].as_array() {
            for instance in instances {
                let Some(id) = instance["vmid"].as_u64() else {
                    continue;
                };
                let Some(kind) = instance["type"].as_str() else {
                    continue;
                };
                let mut labels = with(&with(&node_labels, "vmid", &id.to_string()), "type", kind);
                if let Some(name) = instance["name"].as_str() {
                    labels.insert("name".into(), name.into());
                }
                let sampled = instance["sampled_at_unix_ms"]
                    .as_u64()
                    .and_then(|t| t.checked_mul(1_000_000))
                    .unwrap_or(time);
                builder.cgroup(instance, &labels, sampled);
                let key = format!("{kind}/{id}/{}", instance["cgroup_identity"]);
                if let Some(stamp) = instance["filesystem_report"]["sampled_at_unix_ms"].as_u64() {
                    let changed = builder.encoder.filesystem_samples.get(&key) != Some(&stamp);
                    builder.filesystem(instance, &labels, time, changed);
                    fs_samples.insert(key, stamp);
                }
            }
        }
        builder
            .encoder
            .counters
            .retain(|key, _| builder.seen_counters.contains(key));
        let metrics: Vec<_> = builder.metrics.into_values().collect();
        let resource = BTreeMap::from([
            ("service.name".into(), "homestack-pve-agent".into()),
            ("service.instance.id".into(), node.into()),
            ("host.name".into(), node.into()),
        ]);
        Ok(Batch {
            payload: json!({"resourceMetrics": [{"resource": {"attributes": attributes(&resource)},
            "scopeMetrics": [{"scope": {"name": "homestack.pve", "version": env!("CARGO_PKG_VERSION")}, "metrics": metrics}]}]}),
            points: builder.points,
            filesystem_samples: fs_samples,
        })
    }
}

pub struct Exporter {
    client: Client,
    endpoint: String,
    node: String,
    encoder: Encoder,
    failures: u64,
    previous_success: bool,
    debug: bool,
}

impl Exporter {
    pub fn new(config: &Config) -> Result<Self, String> {
        if config.otlp_endpoint.is_empty() {
            return Err(
                "set otlp_endpoint in /etc/homestack/pve-agent.toml to this node's Collector HTTP /v1/metrics URL"
                    .into(),
            );
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(config.otlp_timeout_seconds))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            endpoint: config.otlp_endpoint.clone(),
            node: config.node()?,
            encoder: Encoder::default(),
            failures: 0,
            previous_success: false,
            debug: config.debug,
        })
    }

    pub fn export(&mut self, report: &Value) -> Result<(), String> {
        let started = Instant::now();
        let batch = self
            .encoder
            .encode(report, &self.node, self.failures, self.previous_success)?;
        let result = self.send(&batch.payload);
        self.previous_success = matches!(result, Ok(true));
        if result.is_ok() {
            self.encoder.filesystem_samples = batch.filesystem_samples;
        }
        if !self.previous_success {
            self.failures = self.failures.saturating_add(1);
        }
        if self.debug {
            eprintln!(
                "[debug] otlp_export node={} points={} elapsed_usec={} success={}",
                self.node,
                batch.points,
                started.elapsed().as_micros(),
                self.previous_success
            );
        }
        result.map(|_| ())
    }

    fn send(&self, payload: &Value) -> Result<bool, String> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(payload)
            .send()
            .map_err(|e| e.without_url().to_string())?;
        if response.status().as_u16() != 200 {
            return Err(format!("Collector returned HTTP {}", response.status()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|s| s.to_str().ok())
            .unwrap_or_default();
        if content_type.split(';').next() != Some("application/json") {
            return Err("Collector did not return OTLP JSON".into());
        }
        let mut bytes = Vec::new();
        response
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 1024 * 1024 {
            return Err("Collector response exceeds 1 MiB".into());
        }
        let reply: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid OTLP JSON response")?;
        if !reply.is_object() {
            return Err("invalid OTLP response object".into());
        }
        if let Some(partial) = reply.get("partialSuccess") {
            // A partial success must never be retried: accepted points would
            // otherwise be submitted again. Record the warning in the journal.
            let rejected = partial["rejectedDataPoints"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| partial["rejectedDataPoints"].as_u64())
                .unwrap_or(0);
            if rejected > 0 || partial["errorMessage"].as_str().is_some_and(|s| !s.is_empty()) {
                eprintln!("OTLP partial success: rejected_data_points={rejected}; consult Collector logs");
            }
            return Ok(rejected == 0);
        }
        Ok(true)
    }
}
