//! A heartbeat through the actual HTTP listener; no remote readiness probes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hs_model::health::{HeartbeatObservation, Liveness, LivenessStatus};

pub(crate) const PROBE_PATH: &str = "/health/heartbeat";

#[derive(Debug)]
pub struct Heartbeat {
    interval: Duration,
    progress: Mutex<(Instant, u64)>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

impl Heartbeat {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            progress: Mutex::new((Instant::now(), 0)),
        }
    }

    pub fn snapshot(&self) -> Liveness {
        let (last, count) = *self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        let age = last.elapsed();
        let status = if age > self.interval.saturating_mul(3) {
            LivenessStatus::Fail
        } else {
            LivenessStatus::Pass
        };
        Liveness {
            status,
            checks: [
                (
                    "heartbeat:age".to_owned(),
                    vec![HeartbeatObservation {
                        status,
                        observed_value: age.as_secs(),
                        observed_unit: Some("s".to_owned()),
                    }],
                ),
                (
                    "heartbeat:count".to_owned(),
                    vec![HeartbeatObservation {
                        status,
                        observed_value: count,
                        observed_unit: None,
                    }],
                ),
            ]
            .into(),
        }
    }

    /// Run only after binding the service's real listener. A successful probe
    /// must pass through the same HTTP workers and configuration read as clients.
    pub async fn run(self: Arc<Self>, mut address: SocketAddr) {
        if address.ip().is_unspecified() {
            address.set_ip(match address.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            });
        }
        let Ok(client) = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.interval.min(Duration::from_secs(2)))
            .build()
        else {
            return;
        };
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + self.interval, self.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let url = format!("http://{address}{PROBE_PATH}");
        loop {
            timer.tick().await;
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|response| response.status() == reqwest::StatusCode::NO_CONTENT)
            {
                let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
                progress.0 = Instant::now();
                progress.1 = progress.1.saturating_add(1);
            }
        }
    }
}
