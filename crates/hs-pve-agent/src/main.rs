//! Read-only cgroup v2 and filesystem snapshot. No PVE API, Perl or subprocesses.
mod config;
mod control;
mod filesystems;
mod limits;
mod otlp;
use config::{Config, Mode};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELP: &str =
    "homestack-pve-agent [--config FILE] [--vmid ID | --path CGROUP] [--root CGROUP_ROOT] [--qga-parallel N] [--debug] [--export]
homestack-pve-agent --daemon [--config /etc/homestack/pve-agent.toml]
homestack-pve-agent --check-config --config FILE
homestack-pve-agent mode [NAME] [--config /etc/homestack/pve-agent.toml]

Default: print one JSON snapshot. --daemon writes the latest snapshot atomically
to snapshot_path and exports OTLP metrics to the configured Collector.
--export also exports a one-shot snapshot; set otlp_endpoint using --config.
--qga-parallel N limits concurrent VM Guest Agent queries (default: 4).
--debug writes sources, values and timings to stderr. JSON remains on stdout.
The daemon starts in default_mode; `mode NAME` switches the running daemon to
another configured mode until it restarts, `mode` alone shows the current one.
LXC filesystems use /proc; VM filesystems use local QGA sockets (3s deadline).
Only running instances have cgroups. No PVE API, Perl or subprocesses are used.
";

#[derive(Default)]
struct Trace {
    debug: bool,
    reads: usize,
    bytes: usize,
    values: usize,
}

impl Trace {
    fn read_io(&mut self, path: &Path) -> io::Result<String> {
        let started = Instant::now();
        let result = fs::read_to_string(path);
        self.reads += 1;
        if let Ok(text) = &result {
            self.bytes += text.len();
        }
        if self.debug {
            eprintln!(
                "[debug] read source={} bytes={} elapsed_usec={} error={}",
                path.display(),
                result.as_ref().map_or(0, |s| s.len()),
                started.elapsed().as_micros(),
                result
                    .as_ref()
                    .err()
                    .map_or_else(|| "none".to_owned(), ToString::to_string)
            );
        }
        result
    }

    fn read(&mut self, path: &Path) -> Result<String, String> {
        self.read_io(path).map_err(|e| e.to_string())
    }

    fn record(&mut self, source: &str, bytes: usize, started: Instant, value: &Value) {
        fn leaves(value: &Value) -> usize {
            match value {
                Value::Object(map) => map.values().map(leaves).sum(),
                Value::Array(list) => list.iter().map(leaves).sum(),
                Value::Null => 0,
                _ => 1,
            }
        }
        let count = leaves(value);
        self.values += count;
        if self.debug {
            eprintln!(
                "[debug] decoded source={source} bytes={bytes} scalar_values={count} elapsed_usec={} values={value}",
                started.elapsed().as_micros()
            );
        }
    }
}

fn numeric_id(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
        .filter(|id| *id > 0)
}

fn discover(root: &Path) -> Result<Vec<(u32, &'static str, PathBuf)>, String> {
    if !root.join("cgroup.controllers").is_file() {
        return Err(format!("{} is not a readable cgroup v2 root", root.display()));
    }
    let mut found = BTreeMap::new();
    // The outer lxc/ID group includes descendant counters. Do not also sum
    // its ns/init.scope descendants or the separate LXC monitor process.
    for (base, suffix, kind) in [
        (root.join("lxc"), "", "lxc"),
        (root.join("qemu.slice"), ".scope", "qemu"),
    ] {
        let entries = match fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", base.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_suffix(suffix).and_then(numeric_id) {
                found.insert((id, kind), entry.path());
            }
        }
    }
    // Some LXC installations use a top-level payload group instead.
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        if let Some(id) = entry
            .file_name()
            .to_string_lossy()
            .strip_prefix("lxc.payload.")
            .and_then(numeric_id)
        {
            found.entry((id, "lxc")).or_insert_with(|| entry.path());
        }
    }
    Ok(found.into_iter().map(|((id, kind), path)| (id, kind, path)).collect())
}

fn atom(s: &str) -> Value {
    if let Ok(n) = s.parse::<u64>() {
        json!(n)
    } else if let Ok(n) = s.parse::<f64>() {
        json!(n)
    } else {
        json!(s)
    }
}

fn keyed(text: &str) -> Value {
    Value::Object(
        text.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                Some((words.next()?.to_owned(), atom(words.next()?)))
            })
            .collect(),
    )
}

fn records(text: &str) -> Value {
    Value::Object(
        text.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let name = words.next()?.to_owned();
                let fields = words
                    .filter_map(|word| {
                        let (key, value) = word.split_once('=')?;
                        Some((key.to_owned(), atom(value)))
                    })
                    .collect();
                Some((name, Value::Object(fields)))
            })
            .collect(),
    )
}

fn snapshot(root: &Path, path: &Path, id: Option<u32>, kind: &str, trace: &mut Trace, read_filesystems: bool) -> Value {
    let instance_started = Instant::now();
    let sampled_at_unix_ms = unix_ms().ok();
    let mut metrics = Map::new();
    let mut errors = Map::new();
    for file in [
        "cgroup.events",
        "cpu.stat",
        "cpu.max",
        "cpu.weight",
        "cpuset.cpus.effective",
        "memory.current",
        "memory.peak",
        "memory.max",
        "memory.high",
        "memory.stat",
        "memory.events",
        "memory.swap.current",
        "memory.swap.max",
        "pids.current",
        "pids.max",
        "cpu.pressure",
        "memory.pressure",
        "io.pressure",
        "io.stat",
    ] {
        let started = Instant::now();
        match trace.read(&path.join(file)) {
            Ok(text) => {
                let value = if file.ends_with(".pressure") || file == "io.stat" {
                    records(&text)
                } else if ["cpu.stat", "memory.stat", "memory.events", "cgroup.events"].contains(&file) {
                    keyed(&text)
                } else if file == "cpu.max" {
                    let mut words = text.split_whitespace();
                    json!({"quota_usec": words.next().map(atom), "period_usec": words.next().map(atom)})
                } else if file == "cpuset.cpus.effective" {
                    json!(text.trim())
                } else {
                    atom(text.trim())
                };
                trace.record(&path.join(file).display().to_string(), text.len(), started, &value);
                metrics.insert(file.to_owned(), value);
            }
            Err(e) => {
                metrics.insert(file.to_owned(), Value::Null);
                errors.insert(file.to_owned(), json!(e.to_string()));
            }
        }
    }
    let effective_limits = limits::collect(root, path, trace);
    // Local-node symlinks supplied by pmxcfs; optional metadata only.
    let name = id.and_then(|id| {
        let (dir, key) = match kind {
            "lxc" => ("lxc", "hostname: "),
            "qemu" => ("qemu-server", "name: "),
            _ => return None,
        };
        let config = trace.read(Path::new(&format!("/etc/pve/{dir}/{id}.conf"))).ok()?;
        // Ignore snapshot and pending sections.
        config
            .lines()
            .take_while(|line| !line.starts_with('['))
            .find_map(|line| line.strip_prefix(key).map(str::to_owned))
    });
    let filesystems = match (kind, id) {
        ("lxc", _) if read_filesystems => filesystems::lxc(path, trace),
        ("lxc", _) => Value::Null,
        // Guest Agent requests run separately through a bounded worker pool.
        ("qemu", Some(_)) => Value::Null,
        _ => json!({"filesystems": null, "error": "instance_type_unknown"}),
    };
    let elapsed = instance_started.elapsed().as_micros();
    if trace.debug {
        eprintln!("[debug] local_instance vmid={id:?} type={kind} elapsed_usec={elapsed}");
    }
    json!({"vmid": id, "type": kind, "name": name, "cgroup_path": path,
        "sampled_at_unix_ms": sampled_at_unix_ms,
        "collection_usec": elapsed, "local_collection_usec": elapsed,
        "guest_agent_collection_usec": null, "metrics": metrics, "read_errors": errors,
        "filesystem_report": filesystems, "effective_limits": effective_limits})
}

struct GuestBatch {
    samples: Vec<(usize, Value, u64)>,
    trace: Trace,
}

fn guest_worker(jobs: &[(usize, u32)], next: &AtomicUsize, node: &str, debug: bool) -> GuestBatch {
    let mut trace = Trace {
        debug,
        ..Trace::default()
    };
    let mut samples = Vec::new();
    while let Some(&(index, id)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
        let started = Instant::now();
        let mut report = filesystems::qemu(id, &mut trace);
        let shares = filesystems::virtiofs(id, node, &mut trace);
        if !shares.is_empty() {
            match report["filesystems"].as_array_mut() {
                Some(list) => list.extend(shares),
                None => report["filesystems"] = json!(shares),
            }
        }
        samples.push((index, report, started.elapsed().as_micros() as u64));
    }
    GuestBatch { samples, trace }
}

fn collect_guests(instances: &mut [Value], parallel: usize, node: &str, trace: &mut Trace) -> Result<(), String> {
    let jobs: Vec<_> = instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            (instance["type"] == "qemu" && instance["filesystem_report"].is_null())
                .then(|| instance["vmid"].as_u64().map(|id| (index, id as u32)))
                .flatten()
        })
        .collect();
    let workers = parallel.min(jobs.len());
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let debug = trace.debug;
    if debug {
        eprintln!(
            "[debug] guest_pool configured_parallel={parallel} workers={workers} vm_count={}",
            jobs.len()
        );
    }
    let batches = if workers <= 1 {
        vec![guest_worker(&jobs, &next, node, debug)]
    } else {
        std::thread::scope(|scope| -> Result<Vec<GuestBatch>, String> {
            let mut handles = Vec::new();
            for worker in 0..workers {
                let jobs = &jobs;
                let next = &next;
                handles.push(
                    std::thread::Builder::new()
                        .name(format!("qga-{worker}"))
                        .spawn_scoped(scope, move || guest_worker(jobs, next, node, debug))
                        .map_err(|e| format!("cannot start Guest Agent worker: {e}"))?,
                );
            }
            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| "Guest Agent worker panicked".to_owned()))
                .collect()
        })?
    };
    for batch in batches {
        trace.reads += batch.trace.reads;
        trace.bytes += batch.trace.bytes;
        trace.values += batch.trace.values;
        for (index, report, elapsed) in batch.samples {
            let instance = &mut instances[index];
            instance["filesystem_report"] = report;
            instance["guest_agent_collection_usec"] = json!(elapsed);
            instance["collection_usec"] = json!(instance["local_collection_usec"].as_u64().unwrap_or(0) + elapsed);
        }
    }
    if debug {
        eprintln!(
            "[debug] guest_pool_complete workers={workers} vm_count={} elapsed_usec={}",
            jobs.len(),
            started.elapsed().as_micros()
        );
    }
    Ok(())
}

#[derive(Default)]
struct Selection {
    path: Option<PathBuf>,
    vmid: Option<u32>,
}

struct CachedFilesystem {
    identity: Option<(u64, u64)>,
    report: Value,
    sampled_at: Instant,
    sampled_at_unix_ms: u64,
}

type Cache = BTreeMap<(u32, &'static str), CachedFilesystem>;

fn unix_ms() -> Result<u64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    u64::try_from(elapsed.as_millis()).map_err(|e| e.to_string())
}

fn collect(config: &Config, mode: (&str, &Mode), selection: &Selection, cache: &mut Cache) -> Result<Value, String> {
    let started = Instant::now();
    let mut trace = Trace {
        debug: config.debug,
        ..Trace::default()
    };
    let mut instances = Vec::new();
    if let Some(path) = &selection.path {
        if !path.join("cgroup.events").is_file() {
            return Err(format!("{} is not a readable cgroup v2 directory", path.display()));
        }
        instances.push(snapshot(&config.cgroup_root, path, None, "unknown", &mut trace, true));
    } else {
        let mut live = Vec::new();
        for (id, kind, path) in discover(&config.cgroup_root)?
            .into_iter()
            .filter(|(id, _, _)| selection.vmid.is_none_or(|wanted| wanted == *id))
        {
            let key = (id, kind);
            live.push(key);
            let identity = fs::metadata(&path).ok().map(|m| (m.dev(), m.ino()));
            let interval = if kind == "qemu" {
                mode.1.qga_interval_seconds
            } else {
                mode.1.lxc_filesystem_interval_seconds
            };
            let previous = cache.get(&key).filter(|entry| {
                identity.is_some()
                    && entry.identity == identity
                    && entry.sampled_at.elapsed() < Duration::from_secs(interval)
            });
            let sample_started = Instant::now();
            let sample_unix_ms = unix_ms()?;
            let mut instance = snapshot(
                &config.cgroup_root,
                &path,
                Some(id),
                kind,
                &mut trace,
                previous.is_none(),
            );
            instance["cgroup_identity"] = json!(identity.map(|(device, inode)| format!("{device}:{inode}")));
            if let Some(entry) = previous {
                instance["filesystem_report"] = entry.report.clone();
                instance["filesystem_report"]["sample_age_seconds"] = json!(entry.sampled_at.elapsed().as_secs());
                instance["filesystem_report"]["sampled_at_unix_ms"] = json!(entry.sampled_at_unix_ms);
            } else {
                cache.insert(
                    key,
                    CachedFilesystem {
                        identity,
                        report: Value::Null,
                        sampled_at: sample_started,
                        sampled_at_unix_ms: sample_unix_ms,
                    },
                );
            }
            instances.push(instance);
        }
        cache.retain(|key, _| live.contains(key));
    }
    collect_guests(&mut instances, config.qga_parallel, &config.node()?, &mut trace)?;
    for instance in &mut instances {
        let Some(id) = instance["vmid"].as_u64().and_then(|n| u32::try_from(n).ok()) else {
            continue;
        };
        let kind = if instance["type"] == "qemu" { "qemu" } else { "lxc" };
        if let Some(entry) = cache.get_mut(&(id, kind)) {
            if entry.report.is_null() {
                entry.report = instance["filesystem_report"].clone();
            }
            instance["filesystem_report"]["sample_age_seconds"] = json!(entry.sampled_at.elapsed().as_secs());
            instance["filesystem_report"]["sampled_at_unix_ms"] = json!(entry.sampled_at_unix_ms);
        }
    }
    if trace.debug {
        eprintln!(
            "[debug] summary instances={} file_reads={} file_bytes={} decoded_scalar_values={} collection_usec={} (includes debug output)",
            instances.len(),
            trace.reads,
            trace.bytes,
            trace.values,
            started.elapsed().as_micros()
        );
    }
    Ok(json!({"schema": 3, "unix_timestamp_ms": unix_ms()?,
        "mode": mode.0, "interval_seconds": mode.1.interval_seconds,
        "collection_usec": started.elapsed().as_micros(), "qga_parallel": config.qga_parallel,
        "instance_count": instances.len(), "instances": instances}))
}

fn write_snapshot(path: &Path, report: &Value) -> Result<(), String> {
    let directory = path.parent().ok_or("snapshot_path has no parent")?;
    fs::create_dir_all(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    let temporary = directory.join(format!(".snapshot-{}-{}.tmp", std::process::id(), unix_ms()?));
    let result = (|| -> Result<(), String> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut file, report).map_err(|e| e.to_string())?;
        writeln!(file).map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// `mode [NAME] [--config FILE]`: switch the running daemon, or show its mode.
fn mode_command(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut config_path = PathBuf::from(config::DEFAULT_PATH);
    let mut wanted = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().ok_or("missing value for --config")?.into(),
            _ if arg.starts_with('-') || wanted.is_some() => return Err(format!("unexpected argument: {arg}")),
            _ => wanted = Some(arg),
        }
    }
    let config = Config::load(&config_path)?;
    config.validate()?;
    if let Some(wanted) = &wanted
        && !config.modes.contains_key(wanted)
    {
        let known: Vec<&str> = config.modes.keys().map(String::as_str).collect();
        return Err(format!("unknown mode {wanted}; modes: {}", known.join(", ")));
    }
    let name = control::request(&config.control_socket(), wanted.as_deref())?;
    match config.modes.get(&name) {
        Some(mode) => println!(
            "{name}: every {} s, LXC filesystems {} s, VM filesystems {} s",
            mode.interval_seconds, mode.lxc_filesystem_interval_seconds, mode.qga_interval_seconds
        ),
        None => println!("{name}"),
    }
    Ok(())
}

fn run() -> Result<(), String> {
    if std::env::args().nth(1).as_deref() == Some("mode") {
        return mode_command(std::env::args().skip(2));
    }
    let mut selection = Selection::default();
    let mut config_path = None;
    let mut root = None;
    let mut parallel = None;
    let mut debug = false;
    let mut daemon = false;
    let mut check_config = false;
    let mut export = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--debug" => {
                debug = true;
                continue;
            }
            "--daemon" => {
                daemon = true;
                continue;
            }
            "--export" => {
                export = true;
                continue;
            }
            "--check-config" => {
                check_config = true;
                continue;
            }
            _ => {}
        }
        let value = args.next().ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--config" => config_path = Some(PathBuf::from(value)),
            "--root" => root = Some(PathBuf::from(value)),
            "--path" => selection.path = Some(PathBuf::from(value)),
            "--vmid" => selection.vmid = Some(numeric_id(&value).ok_or("invalid VMID")?),
            "--qga-parallel" => {
                parallel = Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--qga-parallel must be a positive integer")?,
                )
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    if selection.path.is_some() && selection.vmid.is_some() {
        return Err("--path and --vmid are mutually exclusive".into());
    }
    if daemon && (selection.path.is_some() || selection.vmid.is_some()) {
        return Err("--daemon cannot be combined with --path or --vmid".into());
    }
    if daemon && check_config {
        return Err("--daemon and --check-config are mutually exclusive".into());
    }
    if (daemon || check_config) && config_path.is_none() {
        config_path = Some(config::DEFAULT_PATH.into());
    }
    let mut config = match config_path {
        Some(path) => Config::load(&path)?,
        None => Config::default(),
    };
    if let Some(root) = root {
        config.cgroup_root = root;
    }
    if let Some(parallel) = parallel {
        config.qga_parallel = parallel;
    }
    config.debug |= debug;
    config.validate()?;
    if (daemon || check_config || export) && config.otlp_endpoint.is_empty() {
        return Err(
            "set otlp_endpoint in /etc/homestack/pve-agent.toml to this node's Collector HTTP /v1/metrics URL".into(),
        );
    }
    if check_config {
        println!("Configuration is valid");
        return Ok(());
    }
    let mut cache = Cache::new();
    let mut exporter = if daemon || export {
        Some(otlp::Exporter::new(&config)?)
    } else {
        None
    };
    if !daemon {
        let mode = (config.default_mode.as_str(), &config.modes[&config.default_mode]);
        let report = collect(&config, mode, &selection, &mut cache)?;
        if let Some(exporter) = exporter.as_mut() {
            exporter.export(&report)?;
        }
        let stdout = io::stdout();
        let mut output = stdout.lock();
        serde_json::to_writer_pretty(&mut output, &report).map_err(|e| e.to_string())?;
        writeln!(output).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let directory = config.snapshot_path.parent().ok_or("snapshot_path has no parent")?;
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let lock_path = directory.join(".homestack-pve-agent.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|e| format!("another agent may be using {}: {e}", directory.display()))?;
    let current = Arc::new(Mutex::new(config.default_mode.clone()));
    let (switches, switched) = mpsc::channel();
    control::listen(
        &config.control_socket(),
        config.modes.keys().cloned().collect(),
        Arc::clone(&current),
        switches,
    )?;
    eprintln!(
        "homestack-pve-agent started: snapshot={} control={} mode={} qga_parallel={}",
        config.snapshot_path.display(),
        config.control_socket().display(),
        config.default_mode,
        config.qga_parallel
    );
    loop {
        let started = Instant::now();
        let name = current.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mode = &config.modes[&name];
        let report = collect(&config, (&name, mode), &selection, &mut cache)?;
        write_snapshot(&config.snapshot_path, &report)?;
        if let Some(exporter) = exporter.as_mut()
            && let Err(error) = exporter.export(&report)
        {
            eprintln!("OTLP export failed: {error}; the next cycle will export the latest measurements");
        }
        // A switch starts the next cycle at once, in the new mode.
        if let Some(remaining) = Duration::from_secs(mode.interval_seconds).checked_sub(started.elapsed()) {
            match switched.recv_timeout(remaining) {
                Ok(name) => eprintln!("mode switched to {name}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(remaining),
            }
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("homestack-pve-agent: {e}");
        std::process::exit(1);
    }
}
