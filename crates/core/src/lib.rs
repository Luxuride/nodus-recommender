//! Domain types and the ranking pipeline for the Mastodon recommendation engine.
//!
//! This crate is the dependency-light heart of the workspace: it owns the
//! vocabulary the other crates share (accounts, statuses, candidates, signals)
//! and the scoring/ranking logic that turns ingested activity into ranked
//! recommendations.
//!
//! The design is deliberately unspecified in code until the design documents in
//! this repository's `docs/` directory exist. Treat those documents as the
//! source of truth and let the types here follow them, not the other way
//! around.

#![forbid(unsafe_code)]
