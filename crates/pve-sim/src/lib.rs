//! Simulated Proxmox VE API for tests.
//!
//! `PveSim` serves a subset of `/api2/json` over HTTPS from in-memory state
//! loaded from a scenario. It checks API tokens and their privileges, records
//! every authenticated call as a canonical operation (see
//! `parity/ops.md`), and can fail chosen calls on purpose.
//!
//! Error responses copy the shapes a live PVE 9.2 cluster returns. Behaviour
//! that could not be observed live is marked as such where it is modelled.

mod api;
mod guest;
pub mod ops;
pub mod scenario;
mod state;
mod tls;

use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;

pub use ops::Op;
pub use scenario::ClusterSpec;
use state::ClusterState;

/// Errors starting the simulator.
#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("invalid scenario: {0}")]
    Scenario(String),
    #[error("could not create the simulator certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("could not listen: {0}")]
    Io(#[from] std::io::Error),
}

/// A running simulator. Dropping it does not stop the server; call
/// [`PveSim::shutdown`].
pub struct PveSim {
    state: Arc<Mutex<ClusterState>>,
    handle: Handle<SocketAddr>,
    addr: SocketAddr,
    ca_pem: String,
}

impl PveSim {
    /// Start on a free loopback port.
    pub async fn start(spec: ClusterSpec) -> Result<Self, SimError> {
        Self::start_on(spec, SocketAddr::from(([127, 0, 0, 1], 0))).await
    }

    /// Start on the given address.
    pub async fn start_on(spec: ClusterSpec, addr: SocketAddr) -> Result<Self, SimError> {
        let state = Arc::new(Mutex::new(ClusterState::new(spec).map_err(SimError::Scenario)?));
        let certs = tls::generate()?;
        let rustls = RustlsConfig::from_pem(certs.chain_pem.into_bytes(), certs.key_pem.into_bytes()).await?;
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let handle = Handle::new();
        let server = axum_server::from_tcp_rustls(listener, rustls)?
            .handle(handle.clone())
            .serve(api::router(Arc::clone(&state)).into_make_service());
        tokio::spawn(async move {
            if let Err(error) = server.await {
                tracing::error!(%error, "pve-sim server stopped");
            }
        });
        Ok(Self {
            state,
            handle,
            addr,
            ca_pem: certs.ca_pem,
        })
    }

    /// Base URL, e.g. `https://localhost:43125`. The certificate is valid for
    /// `localhost` and `127.0.0.1`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("https://localhost:{}", self.addr.port())
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// PEM of the CA that signed the server certificate.
    #[must_use]
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// Operations recorded so far, in call order.
    #[must_use]
    pub fn ops(&self) -> Vec<Op> {
        self.lock().ops.clone()
    }

    /// Forget the recorded operations.
    pub fn clear_ops(&self) {
        self.lock().ops.clear();
    }

    /// UPIDs of all known tasks, in creation order.
    #[must_use]
    pub fn task_upids(&self) -> Vec<String> {
        self.lock().task_upids()
    }

    /// Stop serving and wait until the listener is closed.
    pub async fn shutdown(self) {
        self.handle.shutdown();
    }

    fn lock(&self) -> MutexGuard<'_, ClusterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
