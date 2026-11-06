//! Calculator configuration.
//!
//! Same rules as the feeds role: everything through the environment, a typed
//! struct, and a boot failure that names the offending key. The two roles read
//! *different* keys on purpose — the calculator has no `WEB_ORIGINS` and feeds
//! has no `SYNC_MAX_RPS_PER_HOST` — so a copied deployment manifest is caught at
//! startup instead of silently ignoring a setting.

use std::time::Duration;

/// How the calculator runs.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    /// Identity written into `job.locked_by`; defaults to hostname and pid.
    pub worker_id: String,
    pub pool_size: u32,
    /// Jobs claimed per round trip.
    pub batch_size: i64,
    /// Idle sleep between empty rounds.
    pub poll_interval: Duration,
    /// How long a claim stays valid without an update. A replica that dies is
    /// re-queued after this.
    pub lease: Duration,
    /// Replica count as declared by the deployment, used only to refuse a
    /// configuration the process cannot honour (see [`Config::validate`]).
    pub replicas: u32,
    /// Present when shared budgets and locks are available.
    pub redis_url: Option<String>,
    /// Job kinds this replica serves. `None` serves every kind.
    pub kinds: Option<Vec<String>>,
    /// Self-imposed ceiling per host, in requests per second.
    pub max_rps_per_host: f64,
    /// Never spend more than this share of what an instance reports as remaining.
    pub budget_fraction: f64,
    /// Telemetry retention; the retention handler uses it.
    pub event_retention_days: u16,
    /// Status retention.
    pub post_retention_days: u16,
}

impl Config {
    /// Read the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] naming the first unusable key.
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| ConfigError::Missing("DATABASE_URL"))?;
        let worker_id = match std::env::var("CALCULATOR_ID") {
            Ok(id) if !id.trim().is_empty() => id,
            _ => default_worker_id(),
        };
        Ok(Self {
            database_url,
            worker_id,
            pool_size: parse("CALCULATOR_DB_MAX_CONNECTIONS", 4_u32)?,
            batch_size: parse("CALCULATOR_BATCH_SIZE", 8_i64)?,
            poll_interval: Duration::from_millis(parse("CALCULATOR_POLL_MS", 500_u64)?),
            lease: Duration::from_secs(parse("CALCULATOR_LEASE_SECONDS", 600_u64)?),
            replicas: parse("CALCULATOR_REPLICAS", 1_u32)?,
            redis_url: std::env::var("REDIS_URL").ok().filter(|v| !v.is_empty()),
            kinds: std::env::var("CALCULATOR_KINDS")
                .ok()
                .map(|raw| {
                    raw.split(',')
                        .map(|kind| kind.trim().to_owned())
                        .filter(|kind| !kind.is_empty())
                        .collect::<Vec<_>>()
                })
                .filter(|kinds| !kinds.is_empty()),
            max_rps_per_host: parse("SYNC_MAX_RPS_PER_HOST", 1.0_f64)?,
            budget_fraction: parse("SYNC_BUDGET_FRACTION", 0.5_f64)?,
            event_retention_days: parse("EVENT_RETENTION_DAYS", 90_u16)?,
            post_retention_days: parse("POST_RETENTION_DAYS", 14_u16)?,
        })
    }

    /// Reject settings that cannot be honoured.
    ///
    /// The reason to refuse rather than warn: with several replicas and no shared
    /// state, each replica spends the *whole* per-host budget, so the instance sees
    /// N times the request rate we promised. That is exactly the behaviour that gets
    /// a crawler blocked, and it is invisible in our own metrics.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Inconsistent`] for that combination and for values
    /// outside the ranges the queue tolerates.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.replicas > 1 && self.redis_url.is_none() {
            return Err(ConfigError::Inconsistent(
                "CALCULATOR_REPLICAS > 1 without REDIS_URL: per-host budgets would be \
                 multiplied by the replica count",
            ));
        }
        if self.batch_size < 1 || self.batch_size > 100 {
            return Err(ConfigError::Inconsistent(
                "CALCULATOR_BATCH_SIZE must be between 1 and 100",
            ));
        }
        if !(0.0 < self.budget_fraction && self.budget_fraction <= 1.0) {
            return Err(ConfigError::Inconsistent(
                "SYNC_BUDGET_FRACTION must be in (0.0, 1.0]",
            ));
        }
        if self.max_rps_per_host <= 0.0 {
            return Err(ConfigError::Inconsistent(
                "SYNC_MAX_RPS_PER_HOST must be positive",
            ));
        }
        Ok(())
    }
}

fn parse<T: std::str::FromStr>(key: &'static str, default: T) -> Result<T, ConfigError> {
    match std::env::var(key) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(ConfigError::Missing(key)),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|_| ConfigError::Invalid { key, value: raw }),
    }
}

fn default_worker_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_owned());
    format!("{host}:{}", std::process::id())
}

/// Why the configuration is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not set")]
    Missing(&'static str),
    #[error("{key} is not a valid value: {value}")]
    Invalid { key: &'static str, value: String },
    #[error("{0}")]
    Inconsistent(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            database_url: "postgres://unused".to_owned(),
            worker_id: "test:1".to_owned(),
            pool_size: 4,
            batch_size: 8,
            poll_interval: Duration::from_millis(500),
            lease: Duration::from_secs(600),
            replicas: 1,
            redis_url: None,
            kinds: None,
            max_rps_per_host: 1.0,
            budget_fraction: 0.5,
            event_retention_days: 90,
            post_retention_days: 14,
        }
    }

    #[test]
    fn a_single_replica_needs_no_redis() {
        assert!(base().validate().is_ok());
    }

    #[test]
    fn several_replicas_require_shared_state() {
        let config = Config {
            replicas: 3,
            ..base()
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Inconsistent(message)) if message.contains("REDIS_URL")
        ));
    }

    #[test]
    fn several_replicas_with_redis_are_allowed() {
        let config = Config {
            replicas: 3,
            redis_url: Some("redis://localhost".to_owned()),
            ..base()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_budget_share_outside_the_unit_interval_is_refused() {
        let config = Config {
            budget_fraction: 1.5,
            ..base()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_zero_rate_ceiling_is_refused() {
        let config = Config {
            max_rps_per_host: 0.0,
            ..base()
        };
        assert!(config.validate().is_err());
    }
}
