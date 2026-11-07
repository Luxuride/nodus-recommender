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

use nodus_calculator::handlers::{RefreshConfig, SyncConfig};
use nodus_calculator::maintenance::{self, Maintenance};
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
    let refresh = RefreshConfig::new(sealer.clone(), config.max_rps_per_host);
    let sync = SyncConfig::new(sealer, config.max_rps_per_host);
    // One budget for the process, shared through Redis when it is configured: the
    // per-host rate belongs to the instance we crawl, not to a replica. `validate`
    // already refused a replica count above one without Redis.
    let rate = sync.budget_rate();
    let sync = if let Some(url) = config.redis_url.as_deref() {
        match nodus_cache::RedisBudget::connect(url, rate).await {
            Ok(budget) => sync.with_budget(std::sync::Arc::new(budget)),
            Err(error) => {
                error!(%error, "redis request budget is unusable");
                return ExitCode::FAILURE;
            }
        }
    } else {
        // `SyncUser` installs the in-process budget and warns once at startup that
        // per-host budgets are per replica.
        sync
    };
    // Retention windows come from configuration, so shortening one is a deploy,
    // not a code change; the handler reads them at construction and never again.
    let retention = nodus_store::retention::RetentionPolicy {
        post_days: u32::from(config.post_retention_days),
        event_days: u32::from(config.event_retention_days),
    };
    let worker = Worker::new(
        pool.clone(),
        Registry::standard(pool.clone(), sync, refresh, retention),
        config,
    );

    // The tick is a producer, not a handler: it enqueues the passes only a clock asks
    // for. A failure inside it is logged and retried on the next interval, so it
    // cannot take the worker down with it.
    let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
    let tick = tokio::spawn(maintenance::run(
        Maintenance::new(pool),
        MAINTENANCE_INTERVAL,
        async move {
            while stop_rx.changed().await.is_ok() {
                if *stop_rx.borrow() {
                    return;
                }
            }
        },
    ));

    let result = worker.run(shutdown_signal()).await;
    let _ = stop.send(true);
    let _ = tick.await;

    match result {
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

/// How often the maintenance tick looks for work only a clock would ask for.
///
/// Fifteen minutes is the arithmetic of the passes it enqueues: retention runs daily
/// and follow-through recounts are cheap, so anything faster is queue churn, and
/// anything slower makes a fresh deployment wait hours for its first counters.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

async fn shutdown_signal() {
    // A handler that cannot be installed is a degraded shutdown, not a reason to
    // abort: the other signal still works, and the future that failed simply never
    // resolves so `select!` keeps waiting on the one that did.
    let ctrl_c = async {
        if let Err(error) = signal::ctrl_c().await {
            error!(%error, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
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
