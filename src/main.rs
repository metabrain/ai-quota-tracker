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

impl Config {
    pub(crate) fn from_env() -> Self {
        let ttl = env::var("QUOTA_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(300));
        let fetch_timeout = env::var("QUOTA_FETCH_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(15));
        Self {
            socket_path: env::var("QUOTA_SOCKET_PATH")
                .unwrap_or_else(|_| "/dev/shm/ai_quota_cache.sock".to_string()),
            ttl,
            fetch_timeout,
            demo_mode: env::var("QUOTA_DEMO_MODE").as_deref() != Ok("0"),
        }
    }
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
    // Refresh always sets the payload; fall back to an empty one only if the
    // lock raced a shutdown.
    Json(
        cache.payload.clone().unwrap_or(AiQuotaPayload {
            updated_at: now,
            providers: Default::default(),
            errors: [("daemon".to_string(), "cache unavailable".to_string())]
                .into_iter()
                .collect(),
        }),
    )
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
    let listener = match UnixListener::bind(&config.socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("FATAL: cannot bind {}: {e}", config.socket_path);
            std::process::exit(1);
        }
    };
    // Process-level authorization: only this user may connect.
    if let Err(e) = fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600)) {
        eprintln!("FATAL: cannot chmod 600 {}: {e}", config.socket_path);
        let _ = fs::remove_file(&config.socket_path);
        std::process::exit(1);
    }
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
