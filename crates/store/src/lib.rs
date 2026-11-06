//! Postgres persistence: the only crate in the backend workspace that writes SQL.
//!
//! Repositories are grouped by aggregate (`accounts`, `edges`, `similarity`,
//! `feed`, `events`, `jobs`) and each returns domain data, never raw rows.
//! Migrations live in `migrations/` at the workspace root and are applied with
//! `sqlx migrate run` — never by hand, never by an init script.
//!
//! Unimplemented by design: the schema is specified in `docs/03-data-model.md`
//! before any table exists.

#![forbid(unsafe_code)]
