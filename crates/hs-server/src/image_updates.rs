//! Periodic registry checks. Status requests only read the cached result.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use hs_core::inventory::cluster_container_resources;
use hs_core::pve::Pve;
use hs_core::registry;
use hs_model::status::{ImageUpdate, ImageUpdateState, StatusEntry};

use crate::PveHandle;

#[derive(Default)]
struct Check {
    latest: Option<String>,
    checked_at: Option<String>,
    error: Option<String>,
}

pub(crate) struct ImageUpdates {
    checking: AtomicBool,
    checks: RwLock<BTreeMap<String, Check>>,
}

impl Default for ImageUpdates {
    fn default() -> Self {
        Self {
            checking: AtomicBool::new(true),
            checks: RwLock::default(),
        }
    }
}

/// Progress is served from memory so startup need not repeat inventory reads.
#[utoipa::path(get, path = "/v1/images/check", tag = "status", security(("bearer" = [])),
    responses((status = 200, body = hs_model::status::ImageCheckProgress)))]
pub(crate) async fn progress(
    axum::extract::State(state): axum::extract::State<crate::AppState>,
) -> axum::Json<hs_model::status::ImageCheckProgress> {
    axum::Json(hs_model::status::ImageCheckProgress {
        checking: state.image_updates.checking.load(Ordering::Acquire),
    })
}

fn repository(image: &str) -> Option<String> {
    let (host, path, _) = registry::split(image)?;
    Some(format!("{host}/{path}"))
}

impl ImageUpdates {
    pub fn annotate(&self, entry: &mut StatusEntry) {
        let Some(image) = &entry.image else { return };
        let Some(repository) = repository(image) else { return };
        let checks = self.checks.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        let check = checks.get(&repository);
        let available = check
            .and_then(|check| check.latest.as_ref())
            .filter(|latest| registry::newer(latest, image))
            .cloned();
        let state = if available.is_some() {
            ImageUpdateState::Available
        } else if check.is_some_and(|check| check.error.is_some() || check.latest.is_none())
            || registry::split(image)
                .and_then(|(_, _, tag)| registry::version(tag))
                .is_none()
        {
            ImageUpdateState::Unknown
        } else if check.is_some() {
            ImageUpdateState::UpToDate
        } else {
            ImageUpdateState::Checking
        };
        entry.image_update = Some(ImageUpdate {
            state,
            available,
            checked_at: check.and_then(|check| check.checked_at.clone()),
            error: check.and_then(|check| check.error.clone()),
        });
    }
}

/// Check immediately and then hourly. Weak references let a dropped router
/// release its state without a background task keeping it alive.
pub(crate) fn start(pve: &Arc<PveHandle>, updates: &Arc<ImageUpdates>) {
    let pve = Arc::downgrade(pve);
    let updates = Arc::downgrade(updates);
    tokio::spawn(async move {
        loop {
            let (Some(pve), Some(updates)) = (pve.upgrade(), updates.upgrade()) else {
                break;
            };
            updates.checking.store(true, Ordering::Release);
            let delay = match pve.client() {
                Ok(client) => {
                    match repositories(client.as_ref()).await {
                        Ok(repositories) => {
                            for (repository, image) in repositories {
                                let result =
                                    tokio::time::timeout(Duration::from_secs(120), registry::recent(&image, 1, &[]))
                                        .await;
                                let result = match result {
                                    Ok(result) => result,
                                    Err(_) => Err("Image registry check timed out".to_owned()),
                                };
                                let mut checks = updates
                                    .checks
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let check = checks.entry(repository.clone()).or_default();
                                match result {
                                    Ok(images) => {
                                        check.latest = images.first().map(|image| image.image.clone());
                                        check.checked_at = time::OffsetDateTime::now_utc()
                                            .format(&time::format_description::well_known::Rfc3339)
                                            .ok();
                                        check.error = None;
                                    }
                                    Err(error) => {
                                        tracing::warn!(repository, %error, "image update check failed");
                                        check.error = Some(error);
                                    }
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "cannot discover container images"),
                    }
                    Duration::from_secs(3600)
                }
                // Installation can deliver the Proxmox token after startup.
                Err(_) => Duration::from_secs(60),
            };
            updates.checking.store(false, Ordering::Release);
            drop(updates);
            drop(pve);
            tokio::time::sleep(delay).await;
        }
    });
}

async fn repositories(pve: &impl Pve) -> Result<BTreeMap<String, String>, String> {
    let containers = cluster_container_resources(pve)
        .await
        .map_err(|error| error.to_string())?;
    let mut repositories = BTreeMap::new();
    for container in containers {
        if container.pool.is_none() {
            continue;
        }
        let config = match pve
            .get(&format!("/nodes/{}/lxc/{}/config", container.node, container.vmid), &[])
            .await
        {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(vmid = container.vmid, %error, "cannot read container image");
                continue;
            }
        };
        if let Some(image) = registry::deployed_image(&config)
            && let Some(repository) = repository(&image)
        {
            repositories.insert(repository, image);
        }
    }
    Ok(repositories)
}
