//! Persistence for the nodus recommendation service.
//!
//! This crate is the only place that speaks SQL. Everything above it (`nodus-feeds`,
//! `nodus-calculator`) works with the plain Rust rows in [`models`] and the functions in
//! [`users`], [`accounts`] and [`jobs`], so a query plan can be changed without
//! touching business logic — and a business rule can be read without reading SQL.
//! [`crypto`] seals the tokens we hold a copy of, [`retention`] and [`export`] are the two ends of the data lifecycle: the batched
//! deletes that keep the tables inside their size budget, and the per-user document
//! that hands a person everything we still hold.
//!
//! Two conventions matter throughout:
//!
//! * Queries are **runtime** (`sqlx::query_as`), not the `query!` macros. The service
//!   builds its filters dynamically (tier, host, affinity window), and compile-time
//!   macros cannot express that without a combinatorial set of statements.
//! * `citext` columns are selected as `::text`. `citext` exists so that hostnames and
//!   handles compare case-insensitively inside Postgres; sqlx has no codec for it, and
//!   casting keeps the Rust side plain `String`. Inserts rely on the implicit
//!   `text -> citext` assignment cast, which Postgres provides.
//!
//! The schema itself lives in `migrations/` and is applied by [`migrate`]. It is
//! append-only: a released migration is never edited, only followed.

#![forbid(unsafe_code)]

pub mod accounts;
pub mod affinity;
pub mod crypto;
pub mod events;
pub mod export;
pub mod feed;
pub mod jobs;
pub mod models;
pub mod retention;
pub mod similarity;
pub mod users;

use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect to Postgres and apply every pending migration.
///
/// # Errors
///
/// Returns the underlying [`sqlx::Error`] when the database is unreachable, when the
/// URL is malformed, or when a migration fails. A failed migration is fatal by
/// design: the service refuses to serve requests against a schema it does not
/// understand.
pub async fn connect_and_migrate(
    database_url: &str,
    max_connections: u32,
    connect_timeout: std::time::Duration,
) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(connect_timeout)
        .connect(database_url)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Apply the embedded migrations.
///
/// # Errors
///
/// Returns a [`sqlx::Error`] if the migration table cannot be read or a migration
/// statement fails.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(sqlx::Error::from)
}
