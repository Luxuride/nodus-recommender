//! Entry point for the recommendation service's HTTP API.
//!
//! Start-up order is deliberate: configuration, then the database, then the listener.
//! A process that cannot reach Postgres exits instead of binding a port and answering
//! 503s — the platform's restart policy is a better supervisor than a half-alive
//! process.

#![forbid(unsafe_code)]

mod auth;
mod config;
mod error;
mod routes;
mod state;

use crate::config::Config;
use crate::state::AppState;
use std::process::ExitCode;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "nodus-feeds stopped");
            ExitCode::FAILURE
        }
    }
}

/// Boot the service and serve until a shutdown signal arrives.
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    tracing::info!(
        listen = %config.listen_addr,
        origins = config.web_origins.len(),
        sync_enabled = config.sync_enabled,
        public_base_url = %config.public_base_url,
        "starting nodus-feeds"
    );

    let pool = nodus_store::connect_and_migrate(
        &config.database_url,
        config.max_connections,
        config.connect_timeout,
    )
    .await?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "migrations applied");

    let state = AppState::new(pool, config.clone());
    let app = routes::router(state);

    let listener = TcpListener::bind(config.listen_addr).await?;
    tracing::info!("listening on {}", config.listen_addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when the platform asks us to stop.
///
/// Both `SIGINT` and `SIGTERM` take the same path so local `Ctrl-C` exercises the same
/// shutdown sequence production uses.
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
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("interrupt received, draining"),
        () = terminate => tracing::info!("terminate received, draining"),
    }
}

/// JSON logs when `LOG_FORMAT=json`, human-readable otherwise.
///
/// `RUST_LOG` wins when set; the default keeps sqlx quiet, because a pool that logs
/// every statement buries the request line it belongs to.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,nodus_feeds=debug,sqlx=warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if std::env::var("LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json")) {
        builder.json().init();
    } else {
        builder.init();
    }
}
