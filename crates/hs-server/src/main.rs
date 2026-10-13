//! `homestack-server`: see `hs_server::settings` for configuration.

use std::process::ExitCode;

use hs_server::settings::{self, LogFormat};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let env = |name: &str| std::env::var(name).ok();
    let format = match settings::log_format(env) {
        Ok(format) => format,
        Err(error) => {
            eprintln!("homestack-server: {error}");
            return ExitCode::from(2);
        }
    };
    let filter = EnvFilter::try_from_env("HOMESTACK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Text => subscriber.init(),
        LogFormat::Json => subscriber.json().init(),
    }

    let settings = match settings::from_env(env) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(%error, "invalid configuration");
            return ExitCode::from(2);
        }
    };
    let endpoints: Vec<String> = settings
        .server
        .pve
        .endpoints
        .iter()
        .map(|u| u.origin().ascii_serialization())
        .collect();
    let token_id = settings
        .server
        .pve
        .token
        .as_ref()
        .map_or_else(|| "(sent through the API)".to_owned(), |token| token.id().to_owned());
    let instance = settings.server.instance.name.clone();
    let app = match hs_server::app(settings.server) {
        Ok(app) => app,
        Err(error) => {
            tracing::error!(%error, "invalid Proxmox client configuration");
            return ExitCode::from(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(settings.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, listen = %settings.listen, "cannot listen");
            return ExitCode::from(1);
        }
    };
    tracing::info!(listen = %settings.listen, instance, ?endpoints, pve_token = token_id, "homestack-server started");
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(%error, "server failed");
        return ExitCode::from(1);
    }
    tracing::info!("homestack-server stopped");
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
