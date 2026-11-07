//! Runtime configuration, read once at start-up from the environment.
//!
//! Configuration is deliberately parsed into typed fields here rather than read at
//! call sites: a mistyped variable must fail the process at boot, not surface as a
//! 500 on the first request that needed it. The table in
//! `docs/09-operations.md` is the reference for every key below.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use nodus_store::crypto::TokenSealer;

/// Everything the service needs to know to run.
#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Address the HTTP server binds to.
    pub listen_addr: SocketAddr,
    /// Origins allowed to call the API from a browser. Exact matches only: the client
    /// is served from a CDN on a different origin, so this is the only thing keeping a
    /// random page from reading a feed with a stolen token.
    pub web_origins: Vec<String>,
    /// Public base URL of this service, used in log lines and outbound links.
    pub public_base_url: String,
    /// Whether the background sync worker may hold tokens at all.
    pub sync_enabled: bool,
    /// Upper bound on pooled Postgres connections.
    pub max_connections: u32,
    /// Replica count as declared by the deployment, used only to refuse a
    /// configuration the process cannot honour (see [`Config::validate`]).
    pub replicas: u32,
    /// Shared cache endpoint. Absent means the caches are in-process, which is
    /// correct for exactly one replica.
    pub redis_url: Option<String>,
    /// How long a verified token stays cached, locally and in the shared backend.
    pub verify_cache_seconds: u64,
    /// Maximum number of verified tokens the local cache holds per replica.
    pub verify_cache_capacity: usize,
    /// How long a shared post's preview metadata stays cached.
    ///
    /// A shared link is fetched by chat clients and crawlers, not by us, so the
    /// cache is what stops one popular post from turning into an unbounded number
    /// of instance requests. A day is longer than the preview is interesting and
    /// shorter than the post is.
    pub og_cache_seconds: u64,
    /// How long to wait for Postgres on the first connection before giving up.
    pub connect_timeout: Duration,
    /// Key used to encrypt stored instance tokens. Absent when sync is disabled, so a
    /// deployment that never stores a token never has the means to store one.
    pub token_enc_key: Option<TokenSealer>,
}

/// Why configuration could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required variable is absent.
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    /// A variable is present but unusable.
    #[error("environment variable {name} is invalid: {reason}")]
    Invalid {
        /// Variable name.
        name: &'static str,
        /// Why the value was rejected.
        reason: String,
    },
    /// Values that are each valid but cannot work together.
    #[error("{0}")]
    Inconsistent(&'static str),
}

impl Config {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a required value is missing or a supplied value
    /// cannot be parsed. Callers should exit non-zero: a partially configured service
    /// is worse than a stopped one.
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url = required("DATABASE_URL")?;
        let listen_addr = parse_socket_addr(
            optional("LISTEN_ADDR")
                .unwrap_or_else(|| "0.0.0.0:3000".to_owned())
                .as_str(),
        )?;
        let web_origins = optional("WEB_ORIGINS")
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let public_base_url =
            optional("PUBLIC_BASE_URL").unwrap_or_else(|| format!("http://{listen_addr}"));
        let sync_enabled = parse_bool("SYNC_ENABLED", false)?;
        let max_connections = optional("DB_MAX_CONNECTIONS")
            .map(|raw| {
                raw.parse::<u32>().map_err(|e| ConfigError::Invalid {
                    name: "DB_MAX_CONNECTIONS",
                    reason: e.to_string(),
                })
            })
            .transpose()?
            .unwrap_or(10);
        let replicas = parse("FEEDS_REPLICAS", 1_u32)?;
        let redis_url = optional("REDIS_URL");
        let verify_cache_seconds = parse("VERIFY_CACHE_SECONDS", 600_u64)?;
        let verify_cache_capacity = parse("VERIFY_CACHE_CAPACITY", 10_000_usize)?;
        let og_cache_seconds = parse("OG_CACHE_SECONDS", 86_400_u64)?;
        let token_enc_key = match optional("TOKEN_ENC_KEY") {
            Some(raw) => {
                Some(
                    TokenSealer::from_base64(&raw).map_err(|e| ConfigError::Invalid {
                        name: "TOKEN_ENC_KEY",
                        reason: e.to_string(),
                    })?,
                )
            }
            None if sync_enabled => return Err(ConfigError::Missing("TOKEN_ENC_KEY")),
            None => None,
        };
        Ok(Self {
            database_url,
            listen_addr,
            web_origins,
            public_base_url,
            sync_enabled,
            max_connections,
            replicas,
            redis_url,
            verify_cache_seconds,
            verify_cache_capacity,
            og_cache_seconds,
            connect_timeout: Duration::from_secs(10),
            token_enc_key,
        })
    }

    /// Reject settings that cannot be honoured.
    ///
    /// The reason to refuse rather than warn: with several replicas and no shared
    /// cache, every replica verifies a token at its instance independently, so the
    /// instance sees N times the verification traffic and each replica honours a
    /// revocation on its own schedule. One replica without Redis stays supported;
    /// that is what development and small deployments use.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Inconsistent`] when more than one replica is declared
    /// without a shared cache.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.replicas > 1 && self.redis_url.is_none() {
            return Err(ConfigError::Inconsistent(
                "FEEDS_REPLICAS > 1 without REDIS_URL: verified tokens would be cached \
                 per replica and every one of them would call the instance",
            ));
        }
        Ok(())
    }
}

fn parse<T: std::str::FromStr>(name: &'static str, default: T) -> Result<T, ConfigError> {
    match optional(name) {
        None => Ok(default),
        Some(raw) => raw.parse().map_err(|_| ConfigError::Invalid {
            name,
            reason: format!("{raw} is not a valid value"),
        }),
    }
}

fn optional(name: &'static str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    optional(name).ok_or(ConfigError::Missing(name))
}

fn parse_socket_addr(raw: &str) -> Result<SocketAddr, ConfigError> {
    raw.to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .ok_or(ConfigError::Invalid {
            name: "LISTEN_ADDR",
            reason: format!("cannot resolve {raw}"),
        })
}

fn parse_bool(name: &'static str, default: bool) -> Result<bool, ConfigError> {
    match optional(name) {
        None => Ok(default),
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(ConfigError::Invalid {
                name,
                reason: format!("{other} is not a boolean"),
            }),
        },
    }
}
