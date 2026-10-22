//! Language-neutral service telemetry, collected inside the service's mount
//! namespace. No commands, subprocesses, SSH, privileged operations or runtime
//! dependency. See the homestack-service skill's service-status.md contract.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const PATH: &str = "/.well-known/homestack/status";
pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_FILESYSTEMS: usize = 128;
pub const MAX_AGE_SECONDS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Report {
    pub schema: u32,
    /// Process liveness, independent of dependency readiness.
    pub status: Status,
    /// Seconds since this application's telemetry sampler was initialized.
    pub uptime_seconds: u64,
    pub sample_age_seconds: Option<u64>,
    pub load_average: Option<LoadAverage>,
    pub filesystems: Vec<Filesystem>,
    /// A stable error code; never a raw OS error containing private paths.
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
pub struct LoadAverage {
    /// `system`: kernel/procfs view, not asserted to be container-local.
    pub scope: String,
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Filesystem {
    pub mount: String,
    pub filesystem: Option<String>,
    /// Whether mountinfo confirms this exact path as a mount point.
    pub mounted: bool,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Free space available to an unprivileged application user.
    pub available_bytes: Option<u64>,
    pub error: Option<String>,
}

struct Sample {
    at: Option<Instant>,
    load: Option<LoadAverage>,
    filesystems: Vec<Filesystem>,
    error: Option<String>,
}

/// One bounded background sampler per application. HTTP handlers only clone
/// the latest sample: a stalled network filesystem cannot block the handler
/// or create an unbounded number of blocked measurement workers.
#[derive(Clone)]
pub struct Sampler {
    started: Instant,
    sample: Arc<RwLock<Sample>>,
}

impl Sampler {
    pub fn start() -> std::io::Result<Self> {
        let sample = Arc::new(RwLock::new(Sample {
            at: None,
            load: None,
            filesystems: Vec::new(),
            error: Some("pending".to_owned()),
        }));
        let weak = Arc::downgrade(&sample);
        std::thread::Builder::new()
            .name("service-status".to_owned())
            .spawn(move || {
                loop {
                    if weak.strong_count() == 0 {
                        break;
                    }
                    // Do not hold a strong reference or a write lock across OS I/O.
                    let next = collect();
                    let Some(shared) = weak.upgrade() else { break };
                    *shared.write().unwrap_or_else(std::sync::PoisonError::into_inner) = next;
                    drop(shared);
                    std::thread::sleep(Duration::from_secs(5));
                }
            })?;
        Ok(Self {
            started: Instant::now(),
            sample,
        })
    }

    /// `status` is supplied by the application; filesystem errors never change
    /// its liveness or turn a readiness check into a disk-capacity check.
    pub fn report(&self, status: Status) -> Report {
        let sample = self.sample.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        Report {
            schema: 1,
            status,
            uptime_seconds: self.started.elapsed().as_secs(),
            sample_age_seconds: sample.at.map(|at| at.elapsed().as_secs()),
            load_average: sample.load.clone(),
            filesystems: sample.filesystems.clone(),
            error: sample.error.clone(),
        }
    }
}

fn bounded_file(path: &str, limit: u64) -> std::io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?.take(limit + 1).read_to_string(&mut text)?;
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

fn collect() -> Sample {
    // Timestamp the beginning: an NFS call that eventually returns must not
    // make old measurements appear fresh.
    let at = Instant::now();
    let load = bounded_file("/proc/loadavg", 4096).ok().and_then(|text| {
        let values: Vec<f64> = text
            .split_whitespace()
            .take(3)
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        if values.len() != 3 || values.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return None;
        }
        Some(LoadAverage {
            scope: "system".to_owned(),
            one: values[0],
            five: values[1],
            fifteen: values[2],
        })
    });
    let mut mounts = BTreeMap::new();
    let mut text_bytes = 0;
    let mut error = None;
    match bounded_file("/proc/self/mountinfo", 1024 * 1024) {
        Ok(text) => {
            for line in text.lines() {
                let Some((left, right)) = line.split_once(" - ") else {
                    continue;
                };
                let Some(path) = left.split_whitespace().nth(4).and_then(unescape) else {
                    continue;
                };
                let Some(kind) = right.split_whitespace().next() else {
                    continue;
                };
                if data_mount(&path, kind) && std::fs::metadata(&path).map_or(true, |metadata| metadata.is_dir()) {
                    text_bytes += path.len() + kind.len();
                    // Leave room for the two required entries and JSON fields.
                    if mounts.len() >= MAX_FILESYSTEMS - 2 || text_bytes > 4 * 1024 {
                        error = Some("too_many_mounts".to_owned());
                        break;
                    }
                    // Later stacked mounts are the visible ones.
                    mounts.insert(path, kind.to_owned());
                }
            }
        }
        Err(_) => error = Some("mount_inventory_unavailable".to_owned()),
    }
    let mut filesystems: Vec<Filesystem> = mounts.iter().map(|(path, kind)| measure(path, Some(kind))).collect();
    // Never silently report the root's free space as /home/app's free space.
    for required in ["/", "/home/app"] {
        if !mounts.contains_key(required) {
            filesystems.push(Filesystem {
                mount: required.to_owned(),
                filesystem: None,
                mounted: false,
                total_bytes: None,
                used_bytes: None,
                free_bytes: None,
                available_bytes: None,
                error: Some(
                    if error.is_some() {
                        "mount_inventory_unavailable"
                    } else {
                        "not_mounted"
                    }
                    .to_owned(),
                ),
            });
        }
    }
    filesystems.sort_by(|a, b| a.mount.cmp(&b.mount));
    Sample {
        at: Some(at),
        load,
        filesystems,
        error,
    }
}

fn measure(path: &str, kind: Option<&String>) -> Filesystem {
    let mut result = Filesystem {
        mount: path.to_owned(),
        filesystem: kind.cloned(),
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
