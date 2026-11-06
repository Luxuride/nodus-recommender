//! The local cache layered in front of the shared one.

use nodus_cache::Cache;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::identity::{Identity, IdentityCache, VerificationCache};

/// Verified tokens with a process-local cache in front of a shared one.
///
/// The local [`IdentityCache`] is what makes the shared backend worth having: a hot
/// token is answered from memory, and Redis is consulted only when this replica has
/// not seen the token yet. A hit in the shared layer is copied into the local one so
/// the next request on this replica pays no network round trip either.
///
/// Every shared-layer failure is treated as a miss and logged at debug. The cache is
/// an optimisation, not a source of truth: if Redis is down, the instance is asked
/// again and the request still succeeds — a cache outage must never turn into a 500.
#[derive(Debug)]
pub struct SharedIdentityCache {
    pub(super) local: IdentityCache,
    shared: Arc<dyn Cache>,
    ttl: Duration,
}

impl SharedIdentityCache {
    /// Layer a local cache over `shared`, expiring entries after `ttl` and holding
    /// at most `capacity` of them per replica.
    #[must_use]
    pub fn new(shared: Arc<dyn Cache>, ttl: Duration, capacity: usize) -> Self {
        Self {
            local: IdentityCache::new(ttl, capacity),
            shared,
            ttl,
        }
    }
}

/// The shared-cache key for a token fingerprint.
///
/// Hex rather than the raw bytes because the bytes are not valid UTF-8 often
/// enough to be awkward with tooling, and because `redis-cli GET verify:<64 hex>`
/// is something an operator can type.
fn cache_key(key: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(7 + key.len() * 2);
    encoded.push_str("verify:");
    for byte in key {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

impl VerificationCache for SharedIdentityCache {
    fn get<'a>(
        &'a self,
        key: &'a [u8; 32],
    ) -> Pin<Box<dyn Future<Output = Option<Identity>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(identity) = self.local.lookup(key) {
                return Some(identity);
            }
            let encoded = cache_key(key);
            match self.shared.get(&encoded).await {
                Ok(Some(bytes)) => match serde_json::from_slice::<Identity>(&bytes) {
                    Ok(identity) => {
                        self.local.remember(*key, identity.clone());
                        Some(identity)
                    }
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            "shared verification cache held an undecodable entry"
                        );
                        None
                    }
                },
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "shared verification cache read failed; treating as a miss"
                    );
                    None
                }
            }
        })
    }

    fn insert<'a>(
        &'a self,
        key: [u8; 32],
        identity: Identity,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.local.remember(key, identity.clone());
            let encoded = cache_key(&key);
            let bytes = match serde_json::to_vec(&identity) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "verification could not be encoded for the shared cache"
                    );
                    return;
                }
            };
            if let Err(error) = self.shared.set(&encoded, &bytes, self.ttl).await {
                tracing::debug!(%error, "shared verification cache write failed");
            }
        })
    }

    fn forget<'a>(&'a self, key: &'a [u8; 32]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.local.drop_entry(key);
            let encoded = cache_key(key);
            if let Err(error) = self.shared.delete(&encoded).await {
                tracing::debug!(%error, "shared verification cache delete failed");
            }
        })
    }
}
