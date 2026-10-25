//! Cached virtiofs HOME measurements. Status requests never wait for guest commands.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hs_core::virtiofs::{Report, Target};
use hs_model::status::{GlobalStatus, WorkspaceStatus};
use hs_pve::PveClient;

const REFRESH: Duration = Duration::from_secs(60);
const MAX_AGE: Duration = Duration::from_secs(90);
type Key = (String, u32);

#[derive(Clone)]
struct Measurement {
    received: Instant,
    report: Option<Report>,
}

struct Check {
    target: Target,
    requested: Instant,
    measurement: Option<Measurement>,
    failed: bool,
}

#[derive(Default)]
pub(crate) struct Reports {
    checks: Mutex<BTreeMap<Key, Check>>,
}

fn key(target: &Target) -> Key {
    (target.node.clone(), target.vmid)
}

impl Reports {
    fn request(self: &Arc<Self>, pve: Arc<PveClient>, target: Target) {
        let requested = Instant::now();
        {
            let mut checks = self.checks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = checks.get(&key(&target)).filter(|check| check.target == target);
            if previous.is_some_and(|check| check.requested.elapsed() < REFRESH) {
                return;
            }
            let measurement = previous.and_then(|check| check.measurement.clone());
            let failed = previous.is_some_and(|check| check.failed);
            checks.insert(
                key(&target),
                Check {
                    target: target.clone(),
                    requested,
                    measurement,
                    failed,
                },
            );
        }
        let reports = Arc::downgrade(self);
        tokio::spawn(async move {
            let result = hs_core::virtiofs::read(pve.as_ref(), &target).await;
            let Some(reports) = reports.upgrade() else { return };
            let mut checks = reports.checks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(check) = checks.get_mut(&key(&target)) else {
                return;
            };
            if check.target != target || check.requested != requested {
                return;
            }
            match result {
                Ok(report) => {
                    check.measurement = Some(Measurement {
                        received: Instant::now(),
                        report,
                    });
                    check.failed = false;
                }
                Err(error) => {
                    check.failed = true;
                    if matches!(
                        error,
                        hs_core::CoreError::Pve(hs_pve::PveError::Unauthorized | hs_pve::PveError::Forbidden { .. })
                    ) {
                        check.measurement = None;
                    }
                }
            }
        });
    }

    fn cached(&self, target: &Target) -> Option<(Report, bool)> {
        let checks = self.checks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let check = checks.get(&key(target)).filter(|check| check.target == *target)?;
        let measurement = check
            .measurement
            .as_ref()
            .filter(|reading| reading.received.elapsed() < MAX_AGE)?;
        Some((measurement.report.clone()?, check.failed))
    }

    pub fn global(self: &Arc<Self>, pve: Arc<PveClient>, pool: &str, status: &mut GlobalStatus, targets: Vec<Target>) {
        let targets: Vec<_> = targets
            .into_iter()
            .filter(|target| target.pool.as_deref() == Some(pool))
            .collect();
        let active: BTreeSet<_> = targets.iter().map(key).collect();
        self.checks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|key, _| active.contains(key));
        for target in targets {
            self.request(Arc::clone(&pve), target.clone());
            if let Some((report, stale)) = self.cached(&target)
                && let Some(entry) = status
                    .workspaces
                    .iter_mut()
                    .find(|entry| entry.vmid == target.vmid && entry.node == target.node)
            {
                entry.home = Some(report.usage);
                if stale {
                    let warning = "virtiofs data is temporarily unavailable; showing the last successful observation";
                    match &mut entry.warning {
                        Some(previous) => {
                            previous.push_str("; ");
                            previous.push_str(warning);
                        }
                        None => entry.warning = Some(warning.to_owned()),
                    }
                    status.warnings.push(format!("Workspace VM {}: {warning}.", entry.vmid));
                }
            }
        }
        let homes = &mut status.summary.homes;
        let mut quota = 0;
        let mut used = 0;
        let mut free = 0;
        let mut known = 0;
        let mut missing = homes.missing_count;
        for entry in &status.workspaces {
            let home = entry.home.as_ref();
            match home.and_then(|home| home.size_bytes) {
                Some(size) => quota += size,
                None if active.contains(&(entry.node.clone(), entry.vmid)) => missing += 1,
                None => {}
            }
            if let Some(home) = home
                && let (Some(home_used), Some(home_free)) = (home.used_bytes, home.free_bytes)
            {
                used += home_used;
                free += home_free;
                known += 1;
            }
        }
        let all_known = known > 0 && known == status.workspaces.len();
        homes.quota_bytes = quota;
        homes.used_bytes = (known > 0).then_some(used);
        homes.free_bytes = all_known.then_some(free);
        homes.missing_count = missing;
        #[allow(clippy::cast_precision_loss, reason = "Filesystem percentages are approximate")]
        {
            homes.free_percent = (all_known && quota > 0).then(|| 100.0 * free as f64 / quota as f64);
        }
    }

    pub fn workspace(
        self: &Arc<Self>,
        pve: Arc<PveClient>,
        pool: &str,
        status: &mut WorkspaceStatus,
        target: Option<Target>,
    ) {
        let Some(target) = target.filter(|target| target.pool.as_deref() == Some(pool)) else {
            self.checks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&(status.node.clone(), status.vmid));
            return;
        };
        status.home_size_gb = None;
        self.request(pve, target.clone());
        if let Some((report, _)) = self.cached(&target) {
            #[allow(clippy::cast_precision_loss, reason = "Disk sizes are displayed in GiB")]
            {
                status.home_size_gb = report.usage.size_bytes.map(|size| size as f64 / 1024.0_f64.powi(3));
            }
            status.home_mount = Some(report.mount);
            status.home_identity_ok = Some(true);
            status.home_mount_ok = Some(true);
        }
    }
}
