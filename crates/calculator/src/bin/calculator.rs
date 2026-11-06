//! `nodus-calculator` — the recommendation service's heavy half.
//!
//! Every outbound Mastodon request that a browser does not make happens here, along
//! with the similarity and affinity recomputes, model fitting and retention. The
//! user-facing role is `nodus-feeds`; the two share the database and never call each
//! other, so either can be restarted alone.
//!
//! Concurrency model, stated once because it is the question people ask:
//!
//! * **Waiting is async.** Crawling is bound by instance latency and the self-imposed
//!   one-request-per-second-per-host budget, so a replica holds many requests in
//!   flight on a handful of threads.
//! * **Computing is blocking.** Similarity over a large edge set is a CPU loop; it
//!   goes through [`nodus_calculator::handlers::Blocking`] onto `spawn_blocking`
//!   rather than occupying a reactor thread.
//! * **Replicas are the coarse knob.** `M` replicas claim disjoint jobs; horizontal
//!   scaling is cheaper than any amount of in-process tuning, and per-host budgets
//!   stay shared through Redis.
//!
//! See `docs/02-architecture.md` and `docs/04-ingestion.md`.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use nodus_calculator::handlers::SyncConfig;
use nodus_calculator::{Config, Registry, Worker};
use nodus_store::{connect_and_migrate, crypto::TokenSealer};
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            error!(%error, "configuration is unusable");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = config.validate() {
        error!(%error, "configuration is inconsistent");
        return ExitCode::FAILURE;
    }

    // The calculator opens tokens that feeds sealed. Building the sealer here is a
    // boot check: a deployment that forgot `TOKEN_ENC_KEY` fails now, not on the
    // first P0 sync. Without it the worker could still drain non-sync tiers, but a
    // half-configured calculator is a worse surprise than a failed rollout.
    let Ok(encoded) = std::env::var("TOKEN_ENC_KEY") else {
        error!("TOKEN_ENC_KEY is not set; the calculator cannot sync any user");
        return ExitCode::FAILURE;
    };
    let sealer = match TokenSealer::from_base64(&encoded) {
        Ok(sealer) => std::sync::Arc::new(sealer),
        Err(error) => {
            error!(%error, "TOKEN_ENC_KEY is not a usable sealing key");
            return ExitCode::FAILURE;
        }
    };

    // Ten seconds is generous for a private-network primary and short enough that a
    // wrong hostname fails the boot instead of hanging a rollout.
    let pool = match connect_and_migrate(
        &config.database_url,
        config.pool_size,
        std::time::Duration::from_secs(10),
    )
    .await
    {
        Ok(pool) => pool,
        Err(error) => {
            error!(%error, "database is unusable");
            return ExitCode::FAILURE;
        }
    };

    // Migrations are idempotent and shared with feeds; the calculator runs them too so
    // it can start first in a fresh environment.
    if let Err(error) = nodus_store::migrate(&pool).await {
        error!(%error, "migrations failed");
        return ExitCode::FAILURE;
    }

    info!(worker = %config.worker_id, kinds = ?config.kinds, "starting nodus-calculator");
    let sync = SyncConfig::new(sealer, config.max_rps_per_host);
    let worker = Worker::new(pool.clone(), Registry::standard(pool, sync), config);

    match worker.run(shutdown_signal()).await {
        Ok(()) => {
            info!("calculator stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            error!(%error, "calculator stopped on a queue error");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("ctrl-c handler");
    };
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        () = ctrl_c => info!("SIGINT"),
        () = terminate => info!("SIGTERM"),
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,nodus_calculator=debug,sqlx=warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if std::env::var("LOG_FORMAT").is_ok_and(|format| format == "json") {
        builder.json().init();
    } else {
        builder.init();
    }
}
