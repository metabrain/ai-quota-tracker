//! ai-quota-tracker — RAM-only AI quota caching daemon.
//!
//! Serves a unified JSON quota snapshot over a Unix domain socket on tmpfs
//! (`/dev/shm/ai_quota_cache.sock`), so local tools can read AI usage without
//! hitting provider APIs on every call.
//!
//! Providers (each in `providers/`):
//! - OpenAI platform billing (`OPENAI_API_KEY`)
//! - Codex ChatGPT subscription (`~/.codex/auth.json` OAuth)
//! - Anthropic Claude Code subscription (`~/.claude/.credentials.json` OAuth)
//!
//! Nothing is written to disk: the cache lives in RAM, credentials are only
//! ever read from the CLIs' own files, and the socket is removed on shutdown.

mod cache;
mod model;
mod providers;

use axum::{extract::State, routing::get, Json, Router};
use cache::{is_stale, refresh_cache, AppState, Cache};
use model::{unix_now, AiQuotaPayload};
use providers::{
    anthropic::AnthropicProvider, codex::CodexProvider, muse_code::MuseProvider,
    openai::OpenAiProvider, QuotaProvider,
};
use std::{env, fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::{
    net::UnixListener,
    signal::unix::{signal, SignalKind},
    sync::Mutex,
};
use tracing::{info, warn};

/// Runtime configuration, all overridable via env.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub socket_path: String,
    pub ttl: Duration,
    pub fetch_timeout: Duration,
    pub demo_mode: bool,
}

/// Parse a seconds-valued env var. Unset means the default; a *set* value
/// that fails to parse logs a warning and falls back to the default so a
/// typo doesn't silently change the daemon's behaviour.
fn parse_secs_env(name: &str, default: u64) -> Duration {
    match env::var(name) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => {
                warn!(var = %name, value = %raw, default, "invalid value, using default");
                Duration::from_secs(default)
            }
        },
        Err(_) => Duration::from_secs(default),
    }
}

impl Config {
    pub(crate) fn from_env() -> Self {
        let ttl = parse_secs_env("QUOTA_CACHE_TTL_SECS", 300);
        let fetch_timeout = parse_secs_env("QUOTA_FETCH_TIMEOUT_SECS", 15);
        Self {
            socket_path: env::var("QUOTA_SOCKET_PATH")
                .unwrap_or_else(|_| "/dev/shm/ai_quota_cache.sock".to_string()),
            ttl,
            fetch_timeout,
            demo_mode: env::var("QUOTA_DEMO_MODE").as_deref() != Ok("0"),
        }
    }
}

/// The `/quota` fallback contract: the cached payload when there is one, else
/// a synthetic one carrying `now` and a single `daemon: "cache unavailable"`
/// error. Refresh always sets the payload; `None` only happens if the lock
/// raced a shutdown. Pulled out of `get_quota` as a pure function so the
/// fallback shape has a direct, non-async, non-timing-dependent test.
fn quota_or_fallback(payload: Option<AiQuotaPayload>, now: u64) -> AiQuotaPayload {
    payload.unwrap_or(AiQuotaPayload {
        updated_at: now,
        providers: Default::default(),
        errors: [("daemon".to_string(), "cache unavailable".to_string())]
            .into_iter()
            .collect(),
    })
}

async fn get_quota(State(state): State<Arc<AppState>>) -> Json<AiQuotaPayload> {
    let now = unix_now();
    let needs_refresh = {
        let cache = state.cache.lock().await;
        is_stale(&cache, state.config.ttl, now)
    };
    if needs_refresh {
        refresh_cache(&state, false).await;
    }
    let cache = state.cache.lock().await;
    Json(quota_or_fallback(cache.payload.clone(), now))
}

async fn refresh_quota(State(state): State<Arc<AppState>>) -> Json<AiQuotaPayload> {
    refresh_cache(&state, true).await;
    get_quota(State(state)).await
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cache = state.cache.lock().await;
    Json(serde_json::json!({
        "status": "ok",
        "started_at": state.started_at,
        "cache_updated_at": cache.payload.as_ref().map(|p| p.updated_at),
        "providers": state.providers.iter().map(|p| p.name()).collect::<Vec<_>>(),
    }))
}

fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/quota", get(get_quota))
        .route("/quota/refresh", get(refresh_quota))
        .route("/health", get(health))
        .with_state(state)
}

/// Bind the Unix socket so it is `0600` from the instant it exists.
///
/// `bind(2)` applies the process umask to the new socket inode, so a plain
/// `bind` then `chmod` leaves a window where the socket is reachable with
/// umask-default perms — and it lives on a world-writable tmpfs. Tightening
/// umask to `0o177` around the bind closes that window; the explicit
/// `set_permissions` afterwards is a backstop. Startup is single-threaded in
/// practice (only our own tasks have run, none creating mode-sensitive files),
/// so the brief global umask change is safe.
fn bind_socket_0600(path: &str) -> UnixListener {
    let prev_umask = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(path);
    unsafe { libc::umask(prev_umask) };

    let listener = match bound {
        Ok(l) => l,
        Err(e) => {
            eprintln!("FATAL: cannot bind {path}: {e}");
            std::process::exit(1);
        }
    };
    // Backstop: only this user may connect.
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        eprintln!("FATAL: cannot chmod 600 {path}: {e}");
        let _ = fs::remove_file(path);
        std::process::exit(1);
    }
    listener
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    info!(
        socket = %config.socket_path,
        ttl_secs = config.ttl.as_secs(),
        demo_mode = config.demo_mode,
        "starting ai-quota-tracker"
    );

    let client = reqwest::Client::builder()
        .timeout(config.fetch_timeout)
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!("ai-quota-tracker/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client builds");

    let providers: Vec<Arc<dyn QuotaProvider>> = vec![
        Arc::new(OpenAiProvider::new(config.demo_mode)),
        Arc::new(CodexProvider::new(config.demo_mode)),
        Arc::new(AnthropicProvider::new(config.demo_mode)),
        Arc::new(MuseProvider::new(config.demo_mode)),
    ];

    let state = Arc::new(AppState {
        config: config.clone(),
        client,
        providers,
        cache: Mutex::new(Cache {
            payload: None,
            last_updated: 0,
        }),
        refresh_lock: Mutex::new(()),
        started_at: unix_now(),
    });

    // Prime the cache before serving so the first request is instant.
    refresh_cache(&state, true).await;

    let app = build_router(state);

    let _ = fs::remove_file(&config.socket_path);
    let listener = bind_socket_0600(&config.socket_path);
    info!(socket = %config.socket_path, "listening on unix socket");

    let shutdown = async {
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler installs");
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler installs");
        tokio::select! {
            _ = sigint.recv() => info!("SIGINT received, shutting down"),
            _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
        }
    };

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("server error: {e}");
    }

    // Always unlink the socket so the next start never trips over a stale path.
    match fs::remove_file(&config.socket_path) {
        Ok(()) => info!("removed socket {}", config.socket_path),
        Err(e) => warn!("could not remove socket {}: {e}", config.socket_path),
    }
    info!("shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process-global env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_parse_fallback_and_valid_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("QUOTA_CACHE_TTL_SECS");
        env::remove_var("QUOTA_FETCH_TIMEOUT_SECS");
        assert_eq!(Config::from_env().ttl, Duration::from_secs(300));
        assert_eq!(Config::from_env().fetch_timeout, Duration::from_secs(15));

        env::set_var("QUOTA_CACHE_TTL_SECS", "60");
        env::set_var("QUOTA_FETCH_TIMEOUT_SECS", "5");
        assert_eq!(Config::from_env().ttl, Duration::from_secs(60));
        assert_eq!(Config::from_env().fetch_timeout, Duration::from_secs(5));

        // Garbage values warn (visible in test logs) and fall back to defaults.
        env::set_var("QUOTA_CACHE_TTL_SECS", "abc");
        env::set_var("QUOTA_FETCH_TIMEOUT_SECS", "1.5");
        assert_eq!(Config::from_env().ttl, Duration::from_secs(300));
        assert_eq!(Config::from_env().fetch_timeout, Duration::from_secs(15));

        env::remove_var("QUOTA_CACHE_TTL_SECS");
        env::remove_var("QUOTA_FETCH_TIMEOUT_SECS");
    }

    #[test]
    fn quota_or_fallback_synthesizes_cache_unavailable_payload_when_none() {
        let payload = quota_or_fallback(None, 42);
        assert_eq!(payload.updated_at, 42);
        assert!(payload.providers.is_empty());
        assert_eq!(
            payload.errors.get("daemon").map(String::as_str),
            Some("cache unavailable")
        );
        assert_eq!(payload.errors.len(), 1);
    }

    #[test]
    fn quota_or_fallback_preserves_cached_payload_when_some() {
        let cached = AiQuotaPayload {
            updated_at: 100,
            providers: [("stub".to_string(), ProviderQuota::default())]
                .into_iter()
                .collect(),
            errors: Default::default(),
        };
        let payload = quota_or_fallback(Some(cached), 999);
        // The cached payload wins outright: its own updated_at, providers,
        // and (empty) errors — never the fallback's synthetic values.
        assert_eq!(payload.updated_at, 100);
        assert!(payload.providers.contains_key("stub"));
        assert!(payload.errors.is_empty());
    }

    use crate::model::{BillingQuota, ProviderQuota};
    use crate::providers::ProviderError;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    struct StubProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl QuotaProvider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }
        async fn fetch(&self, _client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderQuota {
                billing: Some(BillingQuota {
                    total_granted: 10.0,
                    total_used: 1.0,
                    remaining_balance: 9.0,
                    reset_timestamp: 0,
                }),
                subscription: None,
            })
        }
    }

    fn test_state(ttl_secs: u64) -> (Arc<AppState>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(AppState {
            config: Config {
                socket_path: "test.sock".to_string(),
                ttl: Duration::from_secs(ttl_secs),
                fetch_timeout: Duration::from_secs(1),
                demo_mode: false,
            },
            client: reqwest::Client::builder().build().unwrap(),
            providers: vec![Arc::new(StubProvider {
                calls: calls.clone(),
            })],
            cache: Mutex::new(Cache {
                payload: None,
                last_updated: 0,
            }),
            refresh_lock: Mutex::new(()),
            started_at: 12_345,
        });
        (state, calls)
    }

    async fn get(state: &Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = build_router(Arc::clone(state))
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn quota_refreshes_on_miss_then_serves_cache() {
        let (state, calls) = test_state(300);

        let (status, body) = get(&state, "/quota").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["providers"]["stub"]["billing"]["remaining_balance"],
            9.0
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call within the TTL is served from cache, no extra fetch.
        let (status, _) = get(&state, "/quota").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn quota_refetches_after_ttl_expires() {
        let (state, calls) = test_state(0); // everything is immediately stale
        get(&state, "/quota").await;
        get(&state, "/quota").await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn refresh_endpoint_forces_a_fetch() {
        let (state, calls) = test_state(300);
        get(&state, "/quota").await; // 1
        get(&state, "/quota/refresh").await; // 2: forced despite fresh cache
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn health_reports_state() {
        let (state, _) = test_state(300);
        let (status, body) = get(&state, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["started_at"], 12_345);
        assert_eq!(body["providers"], serde_json::json!(["stub"]));
        assert!(body["cache_updated_at"].is_null());
    }
}
