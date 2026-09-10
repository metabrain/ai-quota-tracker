//! In-memory quota cache with lazy TTL refresh.
//!
//! The cache is RAM-only by design (this daemon exists to avoid disk-backed
//! credential/quota stores). `refresh_cache` fans all providers out in
//! parallel; a failed provider keeps its last-known-good payload instead of
//! poisoning the cache.

use crate::model::{unix_now, AiQuotaPayload};
use crate::providers::QuotaProvider;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::Mutex, task::JoinSet};
use tracing::{error, info, warn};

pub(crate) struct Cache {
    pub payload: Option<AiQuotaPayload>,
    pub last_updated: u64,
    pub refreshing: bool,
}

pub(crate) struct AppState {
    pub config: crate::Config,
    pub client: reqwest::Client,
    pub providers: Vec<Arc<dyn QuotaProvider>>,
    pub cache: Mutex<Cache>,
    pub started_at: u64,
}

/// True when a fresh fetch is needed: never fetched, or older than the TTL.
pub(crate) fn is_stale(cache: &Cache, ttl: Duration, now: u64) -> bool {
    match &cache.payload {
        None => true,
        Some(_) => now.saturating_sub(cache.last_updated) >= ttl.as_secs(),
    }
}

/// Fetch every provider in parallel and rebuild the payload.
/// Providers that fail keep their previous values; a provider that never
/// succeeded lands in `errors`.
pub(crate) async fn refresh_cache(state: &Arc<AppState>) {
    // Only one refresh at a time; concurrent callers reuse the in-flight run.
    {
        let mut cache = state.cache.lock().await;
        if cache.refreshing {
            return;
        }
        cache.refreshing = true;
    }

    let mut set = JoinSet::new();
    for provider in state.providers.clone() {
        let client = state.client.clone();
        set.spawn(async move {
            let name = provider.name().to_string();
            let result = provider.fetch(&client).await;
            (name, result)
        });
    }

    let mut providers = HashMap::new();
    let mut errors = HashMap::new();
    while let Some(done) = set.join_next().await {
        match done {
            Ok((name, Ok(quota))) => {
                info!(provider = %name, "quota refreshed");
                providers.insert(name, quota);
            }
            Ok((name, Err(e))) => {
                warn!(provider = %name, error = %e, "quota fetch failed");
                errors.insert(name, e.to_string());
            }
            Err(e) => error!("provider task panicked: {e}"),
        }
    }

    let now = unix_now();
    let mut cache = state.cache.lock().await;
    match &mut cache.payload {
        Some(existing) => {
            // Preserve last-known-good data for providers that failed.
            for (name, quota) in providers {
                existing.providers.insert(name, quota);
            }
            for name in errors.keys() {
                existing.errors.insert(name.clone(), errors[name].clone());
            }
            // Clear errors for providers that recovered.
            existing.errors.retain(|name, _| errors.contains_key(name));
            existing.updated_at = now;
        }
        None => {
            cache.payload = Some(AiQuotaPayload {
                updated_at: now,
                providers,
                errors,
            });
        }
    }
    cache.last_updated = now;
    cache.refreshing = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderQuota;
    use crate::providers::ProviderError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct StubProvider {
        calls: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }

    #[async_trait]
    impl QuotaProvider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }
        async fn fetch(&self, _client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(ProviderError::NotConfigured("boom"))
            } else {
                Ok(ProviderQuota::default())
            }
        }
    }

    fn test_state(
        ttl_secs: u64,
        fail: &Arc<AtomicBool>,
        calls: &Arc<AtomicUsize>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            config: crate::Config {
                socket_path: "test.sock".to_string(),
                ttl: Duration::from_secs(ttl_secs),
                fetch_timeout: Duration::from_secs(1),
                demo_mode: false,
            },
            client: reqwest::Client::builder().build().unwrap(),
            providers: vec![Arc::new(StubProvider {
                calls: calls.clone(),
                fail: fail.clone(),
            })],
            cache: Mutex::new(Cache {
                payload: None,
                last_updated: 0,
                refreshing: false,
            }),
            started_at: unix_now(),
        })
    }

    #[test]
    fn staleness_logic() {
        let ttl = Duration::from_secs(300);
        let empty = Cache {
            payload: None,
            last_updated: 0,
            refreshing: false,
        };
        assert!(is_stale(&empty, ttl, 1_000_000));

        let fresh = Cache {
            payload: Some(AiQuotaPayload {
                updated_at: 900,
                providers: HashMap::new(),
                errors: HashMap::new(),
            }),
            last_updated: 900,
            refreshing: false,
        };
        assert!(!is_stale(&fresh, ttl, 1_000));
        assert!(is_stale(&fresh, ttl, 1_200));
    }

    #[tokio::test]
    async fn refresh_populates_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let state = test_state(300, &fail, &calls);

        refresh_cache(&state).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let cache = state.cache.lock().await;
        let payload = cache.payload.as_ref().unwrap();
        assert!(payload.providers.contains_key("stub"));
        assert!(payload.errors.is_empty());
    }

    #[tokio::test]
    async fn failed_refresh_preserves_last_good() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let state = test_state(300, &fail, &calls);

        refresh_cache(&state).await;
        fail.store(true, Ordering::SeqCst);
        refresh_cache(&state).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let cache = state.cache.lock().await;
        let payload = cache.payload.as_ref().unwrap();
        // Last-known-good value survived the failed refresh...
        assert!(payload.providers.contains_key("stub"));
        // ...and the failure is reported without wiping the cache.
        assert_eq!(
            payload.errors.get("stub").map(String::as_str),
            Some("not configured: boom")
        );
    }
}
