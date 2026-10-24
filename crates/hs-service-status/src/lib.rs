//! Language-neutral service telemetry, schema 2, collected inside the
//! service's mount namespace and cgroup. No commands, subprocesses, SSH,
//! privileged operations or runtime dependency. See the homestack-service
//! skill's service-status.md contract.
//!
//! One worker collects only when requested. Handlers wait at most two seconds;
//! a stalled network filesystem cannot create more workers or block the
//! listener. Resource figures come from the service's own cgroup v2.
//! `/proc/loadavg` is not used: inside a Proxmox LXC container it is the
//! host's load.

// CPU counts and quotas stay far below the 2^52 where f64 loses integers.
#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Nullable measurements must be present on the wire, even when unavailable.
/// A custom deserializer makes Serde reject a missing Option field.
fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

pub const PATH: &str = "/.well-known/homestack/status";
pub const SCHEMA: u32 = 2;
pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_FILESYSTEMS: usize = 128;
pub const MAX_AGE_SECONDS: u64 = 30;
/// How long a request waits for the measurement worker.
const MAX_WAIT: Duration = Duration::from_secs(2);
/// How long a completed measurement answers further requests.
const REUSE_FOR: Duration = Duration::from_secs(2);
/// cgroup interface files are small; `io.stat` grows by one line per device.
const MAX_CGROUP_FILE: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Report {
    pub schema: u32,
    /// Process liveness, independent of dependency readiness.
    pub status: Status,
    /// Seconds since this application process started, on a monotonic clock.
    pub uptime_seconds: u64,
    /// Monotonic microseconds since process start when the returned
    /// measurement began; `null` while none exists.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub sample_uptime_usec: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub sample_age_seconds: Option<u64>,
    pub filesystems: Vec<Filesystem>,
    pub container: Container,
    /// A stable error code; never a raw OS error containing private paths.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Status {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Filesystem {
    pub mount: String,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub filesystem: Option<String>,
    /// Whether mountinfo confirms this exact path as a mount point.
    pub mounted: bool,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub total_bytes: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub used_bytes: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub free_bytes: Option<u64>,
    /// Free space available to an unprivileged application user.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub available_bytes: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub error: Option<String>,
}

/// Resource usage of the cgroup v2 the service process runs in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Container {
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub cpu: Option<Cpu>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub memory: Option<Memory>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub pressure: Option<Pressure>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub io: Option<Io>,
    /// `cgroup_unavailable` or `host_cgroup`; every member is then `null`.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Cpu {
    /// Logical CPUs the cgroup may use: its cpuset, lowered by any visible
    /// `cpu.max` quota.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub capacity_cpus: Option<f64>,
    /// Cumulative kernel counters; consumers compute rates between reports.
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    /// `null` when the cpu controller is not enabled for the cgroup.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub throttling: Option<Throttling>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Throttling {
    pub periods: u64,
    pub throttled_periods: u64,
    pub throttled_usec: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Memory {
    /// `memory.current`: everything charged to the cgroup, page cache included.
    pub usage_bytes: u64,
    /// The lowest visible `memory.max`; `null` when unlimited or set outside
    /// the visible hierarchy.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub limit_bytes: Option<u64>,
    /// Anonymous memory of all processes in the cgroup.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub anon_bytes: Option<u64>,
    /// Page cache, mostly reclaimable.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub file_bytes: Option<u64>,
    /// Resident set of the service process alone.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub process_rss_bytes: Option<u64>,
    /// Cumulative `memory.events` counters.
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub events: Option<MemoryEvents>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemoryEvents {
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

/// Pressure stall information: time stalled on contention, not utilization.
/// A resource is `null` when the kernel does not provide PSI for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Pressure {
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub cpu: Option<Psi>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub memory: Option<Psi>,
    #[serde(deserialize_with = "required_option")]
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub io: Option<Psi>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Psi {
    pub some: PsiLine,
    /// Only available where the kernel reports a full-stall line.
    pub full: Option<PsiLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PsiLine {
    /// Percent of wall time stalled over the last 10, 60 and 300 seconds.
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_usec: u64,
}

/// Cumulative block I/O counters, summed over devices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Io {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
}

struct Sample {
    at: Option<Instant>,
    filesystems: Vec<Filesystem>,
    container: Container,
    error: Option<String>,
}

struct Shared {
    sample: Sample,
    finished: Option<Instant>,
    in_flight: bool,
}

/// A single worker sleeps on its channel until an HTTP request needs a
/// measurement; no timer runs while nobody asks. The short lock is never held
/// across OS I/O or an await.
#[derive(Clone)]
pub struct Telemetry {
    started: Instant,
    shared: Arc<RwLock<Shared>>,
    requests: mpsc::SyncSender<()>,
    completed: tokio::sync::watch::Receiver<()>,
}

impl Telemetry {
    /// `started` is when the application process started.
    pub fn start(started: Instant) -> std::io::Result<Self> {
        let shared = Arc::new(RwLock::new(Shared {
            sample: Sample {
                at: None,
                filesystems: ["/", "/home/app"]
                    .map(|path| unmounted(path, "mount_inventory_unavailable"))
                    .into(),
                container: Container {
                    error: Some("cgroup_unavailable".to_owned()),
                    ..Container::default()
                },
                error: Some("pending".to_owned()),
            },
            finished: None,
            in_flight: false,
        }));
        let (requests, receiver) = mpsc::sync_channel(1);
        let (completed, updates) = tokio::sync::watch::channel(());
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("service-status".to_owned())
            .spawn(move || {
                let mut cgroup = None;
                while receiver.recv().is_ok() {
                    let next = collect(&mut cgroup);
                    let mut shared = worker.write().unwrap_or_else(PoisonError::into_inner);
                    shared.sample = next;
                    shared.finished = Some(Instant::now());
                    shared.in_flight = false;
                    // Notify under the lock so another request cannot miss completion.
                    completed.send_replace(());
                }
            })?;
        Ok(Self {
            started,
            shared,
            requests,
            completed: updates,
        })
    }

    /// `status` is supplied by the application; measurement errors never
    /// change its liveness or turn a readiness check into a capacity check.
    pub async fn report(&self, status: Status) -> Report {
        let mut completed = self.completed.clone();
        let wait = {
            let mut shared = self.shared.write().unwrap_or_else(PoisonError::into_inner);
            // Match the watched version to this cache state under the same lock.
            completed.borrow_and_update();
            if shared.finished.is_some_and(|at| at.elapsed() < REUSE_FOR) {
                false
            } else {
                if !shared.in_flight {
                    shared.in_flight = self.requests.try_send(()).is_ok();
                }
                shared.in_flight
            }
        };
        if wait {
            let _ = tokio::time::timeout(MAX_WAIT, completed.changed()).await;
        }
        let shared = self.shared.read().unwrap_or_else(PoisonError::into_inner);
        let sample = &shared.sample;
        Report {
            schema: SCHEMA,
            status,
            uptime_seconds: self.started.elapsed().as_secs(),
            sample_uptime_usec: sample
                .at
                .and_then(|at| u64::try_from(at.saturating_duration_since(self.started).as_micros()).ok()),
            sample_age_seconds: sample.at.map(|at| at.elapsed().as_secs()),
            filesystems: sample.filesystems.clone(),
            container: sample.container.clone(),
            error: sample.error.clone(),
        }
    }
}

fn bounded_file(path: impl AsRef<Path>, limit: u64) -> std::io::Result<String> {
    let mut text = String::new();
    File::open(path)?.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(std::io::Error::other("measurement exceeds limit"));
    }
    Ok(text)
}

fn unescape(value: &str) -> Option<String> {
    let mut result = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        result.push(if byte == b'\\' {
            match (bytes.next(), bytes.next(), bytes.next()) {
                (Some(b'0'), Some(b'4'), Some(b'0')) => b' ',
                (Some(b'0'), Some(b'1'), Some(b'1')) => b'\t',
                (Some(b'0'), Some(b'1'), Some(b'2')) => b'\n',
                (Some(b'1'), Some(b'3'), Some(b'4')) => b'\\',
                _ => return None,
            }
        } else {
            byte
        });
    }
    String::from_utf8(result).ok()
}

/// Mount root, mount point and filesystem type of each mountinfo line.
fn mountinfo_entries(text: &str) -> Option<Vec<(String, String, &str)>> {
    text.lines()
        .map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mut fields = left.split_whitespace().skip(3);
            let root = unescape(fields.next()?)?;
            let path = unescape(fields.next()?)?;
            Some((root, path, right.split_whitespace().next()?))
        })
        .collect()
}

fn data_mount(path: &str, kind: &str) -> bool {
    if path == "/" || path == "/home/app" {
        return true;
    }
    if ["/proc", "/sys", "/dev", "/run"]
        .iter()
        .any(|base| path == *base || path.starts_with(&format!("{base}/")))
    {
        return false;
    }
    !matches!(
        kind,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "cgroup"
            | "cgroup2"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "pstore"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "autofs"
    )
}

fn collect(cgroup: &mut Option<Cgroup>) -> Sample {
    // Timestamp the beginning: an NFS call that eventually returns must not
    // make old measurements appear fresh.
    let at = Instant::now();
    let mountinfo = bounded_file("/proc/self/mountinfo", 1024 * 1024).ok();
    let (filesystems, error) = filesystems(mountinfo.as_deref());
    // Resolve once, and again only when the directory stops answering.
    if cgroup
        .as_ref()
        .is_some_and(|cgroup| !cgroup.dir.join("cgroup.type").exists())
    {
        *cgroup = None;
    }
    let mut unavailable = "cgroup_unavailable";
    if cgroup.is_none()
        && let Some(mountinfo) = mountinfo.as_deref()
    {
        match Cgroup::resolve(mountinfo) {
            Ok(resolved) => *cgroup = Some(resolved),
            Err(error) => unavailable = error,
        }
    }
    let container = cgroup.as_ref().map_or_else(
        || Container {
            error: Some(unavailable.to_owned()),
            ..Container::default()
        },
        container,
    );
    Sample {
        at: Some(at),
        filesystems,
        container,
        error: error.map(str::to_owned),
    }
}

fn filesystems(mountinfo: Option<&str>) -> (Vec<Filesystem>, Option<&'static str>) {
    let Some(entries) = mountinfo
        .and_then(mountinfo_entries)
        .filter(|entries| entries.iter().any(|(_, path, _)| path == "/"))
    else {
        return (
            ["/", "/home/app"]
                .map(|path| unmounted(path, "mount_inventory_unavailable"))
                .into(),
            Some("mount_inventory_unavailable"),
        );
    };
    // Resolve stacked mounts before filtering or limiting the response. The
    // complete inventory was already bounded to 1 MiB by the caller.
    let mut mounts: BTreeMap<_, _> = entries.into_iter().map(|(_, path, kind)| (path, kind)).collect();
    let mut filesystems = Vec::new();
    let mut text_bytes = 0;
    // Reserve both required entries first, regardless of their inventory order
    // or filesystem type. Absence is known even when other mounts are omitted.
    for required in ["/", "/home/app"] {
        filesystems.push(if let Some(kind) = mounts.remove(required) {
            text_bytes += required.len() + kind.len();
            measure(required, kind)
        } else {
            unmounted(required, "not_mounted")
        });
    }
    let mut error = None;
    for (path, kind) in mounts {
        // File bind mounts are excluded, but an unreadable mount remains visible.
        if !data_mount(&path, kind) || std::fs::metadata(&path).is_ok_and(|metadata| !metadata.is_dir()) {
            continue;
        }
        if filesystems.len() >= MAX_FILESYSTEMS || text_bytes + path.len() + kind.len() > 4 * 1024 {
            error = Some("too_many_mounts");
            continue;
        }
        text_bytes += path.len() + kind.len();
        filesystems.push(measure(&path, kind));
    }
    filesystems.sort_by(|a, b| a.mount.cmp(&b.mount));
    (filesystems, error)
}

fn unmounted(path: &str, error: &str) -> Filesystem {
    Filesystem {
        mount: path.to_owned(),
        filesystem: None,
        mounted: false,
        total_bytes: None,
        used_bytes: None,
        free_bytes: None,
        available_bytes: None,
        error: Some(error.to_owned()),
    }
}

fn measure(path: &str, kind: &str) -> Filesystem {
    let mut result = Filesystem {
        mount: path.to_owned(),
        filesystem: Some(kind.to_owned()),
        mounted: true,
        total_bytes: None,
        used_bytes: None,
        free_bytes: None,
        available_bytes: None,
        error: None,
    };
    match rustix::fs::statvfs(path) {
        Ok(stat) => {
            let unit = stat.f_frsize;
            let sizes = (|| {
                Some((
                    stat.f_blocks.checked_mul(unit)?,
                    stat.f_blocks.checked_sub(stat.f_bfree)?.checked_mul(unit)?,
                    stat.f_bfree.checked_mul(unit)?,
                    stat.f_bavail.checked_mul(unit)?,
                ))
            })();
            if let Some((total, used, free, available)) =
                sizes.filter(|(total, _, free, available)| unit > 0 && *free <= *total && *available <= *free)
            {
                result.total_bytes = Some(total);
                result.used_bytes = Some(used);
                result.free_bytes = Some(free);
                result.available_bytes = Some(available);
            } else {
                result.error = Some("invalid_statistics".to_owned());
            }
        }
        Err(_) => result.error = Some("measurement_unavailable".to_owned()),
    }
    result
}

/// The process's cgroup v2 directory, below the cgroup2 mount point `top`.
struct Cgroup {
    top: PathBuf,
    dir: PathBuf,
}

impl Cgroup {
    /// The unified `0::` path of `/proc/self/cgroup` is relative to the
    /// cgroup namespace root, as is the root of a cgroup2 mount made inside
    /// the namespace: a Proxmox LXC container sees its own cgroup as `/`.
    fn resolve(mountinfo: &str) -> Result<Self, &'static str> {
        let membership = bounded_file("/proc/self/cgroup", MAX_CGROUP_FILE).map_err(|_| "cgroup_unavailable")?;
        let path = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or("cgroup_unavailable")?;
        let (top, relative) = mountinfo_entries(mountinfo)
            .ok_or("cgroup_unavailable")?
            .into_iter()
            .filter(|(_, _, kind)| *kind == "cgroup2")
            .find_map(|(root, mount, _)| {
                // A path outside the mount (e.g. "/../sibling") has no visible directory.
                let relative = Path::new(path).strip_prefix(&root).ok()?;
                relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                    .then(|| (PathBuf::from(mount), relative.to_path_buf()))
            })
            .ok_or("cgroup_unavailable")?;
        let dir = top.join(relative);
        // The real hierarchy root has no cgroup.type: its counters cover the whole host.
        if !dir.join("cgroup.type").exists() {
            return Err("host_cgroup");
        }
        Ok(Self { top, dir })
    }

    fn read(&self, file: &str) -> Option<String> {
        bounded_file(self.dir.join(file), MAX_CGROUP_FILE).ok()
    }

    /// The cgroup and its ancestors up to the visible root.
    fn ancestors(&self) -> impl Iterator<Item = &Path> {
        self.dir.ancestors().take_while(|dir| dir.starts_with(&self.top))
    }
}

/// `key value` lines, as in `cpu.stat`, `memory.stat` and `memory.events`.
fn flat_keyed(text: &str) -> BTreeMap<&str, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            Some((key, value.trim().parse().ok()?))
        })
        .collect()
}

fn container(cgroup: &Cgroup) -> Container {
    let cpu = cgroup.read("cpu.stat").and_then(|text| {
        let fields = flat_keyed(&text);
        Some(Cpu {
            capacity_cpus: cpu_capacity(cgroup).map(|cpus| (cpus * 1000.0).round() / 1000.0),
            usage_usec: *fields.get("usage_usec")?,
            user_usec: *fields.get("user_usec")?,
            system_usec: *fields.get("system_usec")?,
            // Present only with the cpu controller enabled.
            throttling: (|| {
                Some(Throttling {
                    periods: *fields.get("nr_periods")?,
                    throttled_periods: *fields.get("nr_throttled")?,
                    throttled_usec: *fields.get("throttled_usec")?,
                })
            })(),
        })
    });
    let memory = cgroup
        .read("memory.current")
        .and_then(|text| text.trim().parse().ok())
        .map(|usage_bytes| {
            let stat = cgroup.read("memory.stat");
            let stat = stat.as_deref().map(flat_keyed);
            let events = cgroup.read("memory.events");
            let events = events.as_deref().map(flat_keyed);
            Memory {
                usage_bytes,
                limit_bytes: lowest_limit(cgroup, "memory.max", |value| value.parse().ok()),
                anon_bytes: stat.as_ref().and_then(|stat| stat.get("anon").copied()),
                file_bytes: stat.as_ref().and_then(|stat| stat.get("file").copied()),
                process_rss_bytes: process_rss(),
                events: events.and_then(|events| {
                    Some(MemoryEvents {
                        high: *events.get("high")?,
                        max: *events.get("max")?,
                        oom: *events.get("oom")?,
                        oom_kill: *events.get("oom_kill")?,
                    })
                }),
            }
        });
    let pressure = Pressure {
        cpu: cgroup.read("cpu.pressure").and_then(|text| psi(&text)),
        memory: cgroup.read("memory.pressure").and_then(|text| psi(&text)),
        io: cgroup.read("io.pressure").and_then(|text| psi(&text)),
    };
    let io = cgroup.read("io.stat").and_then(|text| io_stat(&text));
    Container {
        cpu,
        memory,
        pressure: Some(pressure),
        io,
        error: None,
    }
}

/// The smallest limit set on the cgroup or a visible ancestor; `max` means
/// none. Limits a container manager sets above the cgroup namespace stay
/// invisible.
fn lowest_limit<T: PartialOrd>(cgroup: &Cgroup, file: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    cgroup
        .ancestors()
        .filter_map(|dir| bounded_file(dir.join(file), MAX_CGROUP_FILE).ok())
        .filter_map(|text| parse(text.trim()))
        .reduce(|lowest, limit| if limit < lowest { limit } else { lowest })
}

fn cpu_capacity(cgroup: &Cgroup) -> Option<f64> {
    // cpuset.cpus.effective already accounts for the ancestors' cpusets.
    let cpus = cgroup
        .read("cpuset.cpus.effective")
        .and_then(|text| count_cpus(text.trim()))
        .or_else(|| std::thread::available_parallelism().ok().map(std::num::NonZero::get))? as f64;
    // cpu.max is "$QUOTA $PERIOD" or "max $PERIOD".
    let quota = lowest_limit(cgroup, "cpu.max", |value| {
        let (quota, period) = value.split_once(' ')?;
        let (quota, period) = (quota.parse::<u64>().ok()?, period.parse::<u64>().ok()?);
        (period > 0).then(|| quota as f64 / period as f64)
    });
    Some(quota.map_or(cpus, |quota| quota.min(cpus)))
}

/// A cpu list such as `0-3,6`.
fn count_cpus(cpu_list: &str) -> Option<usize> {
    cpu_list
        .split(',')
        .filter(|range| !range.is_empty())
        .try_fold(0, |count, range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            let (first, last) = (first.parse::<usize>().ok()?, last.parse::<usize>().ok()?);
            Some(count + last.checked_sub(first)? + 1)
        })
}

fn process_rss() -> Option<u64> {
    let status = bounded_file("/proc/self/status", MAX_CGROUP_FILE).ok()?;
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    kib.checked_mul(1024)
}

/// `some avg10=0.00 avg60=0.00 avg300=0.00 total=0` and optionally a `full` line.
fn psi(text: &str) -> Option<Psi> {
    let line = |kind: &str| -> Option<PsiLine> {
        let fields: BTreeMap<&str, &str> = text
            .lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))?
            .split_whitespace()
            .filter_map(|field| field.split_once('='))
            .collect();
        let average = |key: &str| {
            fields
                .get(key)?
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
        };
        Some(PsiLine {
            avg10: average("avg10")?,
            avg60: average("avg60")?,
            avg300: average("avg300")?,
            total_usec: fields.get("total")?.parse().ok()?,
        })
    };
    Some(Psi {
        some: line("some")?,
        full: line("full"),
    })
}

/// `MAJ:MIN rbytes=… wbytes=… rios=… wios=… dbytes=… dios=…` per device,
/// summed; an overflowing sum is unavailable.
fn io_stat(text: &str) -> Option<Io> {
    let mut total = Io {
        read_bytes: 0,
        write_bytes: 0,
        read_ops: 0,
        write_ops: 0,
    };
    for (key, value) in text
        .lines()
        .flat_map(|line| line.split_whitespace().skip(1))
        .filter_map(|field| field.split_once('='))
    {
        let counter = match key {
            "rbytes" => &mut total.read_bytes,
            "wbytes" => &mut total.write_bytes,
            "rios" => &mut total.read_ops,
            "wios" => &mut total.write_ops,
            _ => continue,
        };
        *counter = counter.checked_add(value.parse().ok()?)?;
    }
    Some(total)
}
