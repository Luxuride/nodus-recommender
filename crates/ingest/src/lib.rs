//! Fediverse data ingestion for the Mastodon recommendation engine.
//!
//! This crate is responsible for getting data *in*: talking to Mastodon
//! instances (REST and streaming APIs, `ActivityPub` where needed), normalizing
//! what comes back, and writing it to whatever store the design documents
//! settle on. It should not contain ranking logic — that belongs in
//! `nodus-core`.
//!
//! Nothing here is implemented yet; see `docs/` for the design.

#![forbid(unsafe_code)]
