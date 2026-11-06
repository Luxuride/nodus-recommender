//! Tests for the local cache and for the layering in front of the shared one.

use super::identity::{Identity, IdentityCache, VerificationCache};
use super::shared::SharedIdentityCache;
use nodus_cache::{Cache, CacheError};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Drive a ready future to completion without a runtime.
fn poll<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => panic!("the local cache never pends"),
    }
}

fn identity(acct: &str) -> Identity {
    Identity {
        mastodon_account_id: 1,
        acct: acct.to_owned(),
    }
}

#[test]
fn an_inserted_identity_is_returned() {
    let cache = IdentityCache::new(Duration::from_secs(60), 4);
    let key = [7_u8; 32];
    poll(cache.insert(key, identity("a@b")));
    assert_eq!(poll(cache.get(&key)), Some(identity("a@b")));
}

#[test]
fn an_expired_identity_is_not_returned() {
    let cache = IdentityCache::new(Duration::from_millis(1), 4);
    let key = [7_u8; 32];
    poll(cache.insert(key, identity("a@b")));
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(poll(cache.get(&key)), None);
}

#[test]
fn the_cache_never_exceeds_its_capacity() {
    let cache = IdentityCache::new(Duration::from_secs(60), 2);
    for byte in 0..5_u8 {
        poll(cache.insert([byte; 32], identity("a@b")));
    }
    assert!(cache.len() <= 2, "len was {}", cache.len());
}

#[test]
fn a_forgotten_identity_is_gone() {
    let cache = IdentityCache::new(Duration::from_secs(60), 4);
    let key = [3_u8; 32];
    poll(cache.insert(key, identity("a@b")));
    poll(cache.forget(&key));
    assert_eq!(poll(cache.get(&key)), None);
}

/// A [`Cache`] that remembers one value and counts every operation, so a test
/// can assert a layer was or was not consulted without a Redis server or a
/// runtime beyond the one `#[tokio::test]` provides.
#[derive(Debug)]
struct CountingCache {
    reads: AtomicUsize,
    writes: AtomicUsize,
    value: Mutex<Option<Vec<u8>>>,
    fail_reads: bool,
}

impl CountingCache {
    fn new(fail_reads: bool) -> Self {
        Self {
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            value: Mutex::new(None),
            fail_reads,
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }
}

impl Cache for CountingCache {
    fn get<'a>(
        &'a self,
        _key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send + 'a>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let answer = if self.fail_reads {
            Err(CacheError::Unavailable("test backend is down".to_owned()))
        } else {
            // A poisoned test lock is not worth distinguishing from empty.
            match self.value.lock() {
                Ok(slot) => Ok((*slot).clone()),
                Err(_) => Ok(None),
            }
        };
        Box::pin(std::future::ready(answer))
    }

    fn set<'a>(
        &'a self,
        _key: &'a str,
        value: &'a [u8],
        _ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.value.lock() {
            *slot = Some(value.to_vec());
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn delete<'a>(
        &'a self,
        _key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        if let Ok(mut slot) = self.value.lock() {
            *slot = None;
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

fn layered(shared: Arc<CountingCache>) -> SharedIdentityCache {
    SharedIdentityCache::new(shared, Duration::from_secs(60), 4)
}

#[tokio::test]
async fn a_local_hit_does_not_touch_the_shared_layer() {
    let shared = Arc::new(CountingCache::new(false));
    let cache = layered(Arc::clone(&shared));
    let key = [9_u8; 32];

    cache.insert(key, identity("a@b")).await;
    let writes_after_insert = shared.writes();
    let reads_after_insert = shared.reads();

    assert_eq!(cache.get(&key).await, Some(identity("a@b")));
    assert_eq!(cache.get(&key).await, Some(identity("a@b")));

    assert_eq!(shared.reads(), reads_after_insert, "local hit read shared");
    assert_eq!(shared.writes(), writes_after_insert, "a read wrote shared");
}

#[tokio::test]
async fn a_shared_hit_populates_the_local_layer() {
    let shared = Arc::new(CountingCache::new(false));
    let cache = layered(Arc::clone(&shared));
    let key = [5_u8; 32];
    let encoded = serde_json::to_vec(&identity("shared@host")).expect("identity encodes");
    shared
        .set("verify:any", &encoded, Duration::from_secs(60))
        .await
        .expect("fake cache always writes");

    assert_eq!(cache.get(&key).await, Some(identity("shared@host")));
    assert_eq!(shared.reads(), 1);
    assert_eq!(cache.local.len(), 1, "the shared hit was not remembered");

    // The second read is answered locally, so the shared layer is not asked again.
    assert_eq!(cache.get(&key).await, Some(identity("shared@host")));
    assert_eq!(shared.reads(), 1);
}

#[tokio::test]
async fn a_shared_error_is_a_miss_and_not_a_panic() {
    let shared = Arc::new(CountingCache::new(true));
    let cache = layered(Arc::clone(&shared));

    assert_eq!(cache.get(&[1_u8; 32]).await, None);
    assert_eq!(shared.reads(), 1);
    assert_eq!(
        cache.local.len(),
        0,
        "a failed read must not remember anything"
    );
}
