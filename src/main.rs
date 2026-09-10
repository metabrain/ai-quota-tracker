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
    anthropic::AnthropicProvider, codex::CodexProvider, openai::OpenAiProvider, QuotaProvider,
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
        refresh_cache(&state).await;
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
    refresh_cache(&state).await;
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
    ];

    let state = Arc::new(AppState {
        config: config.clone(),
        client,
        providers,
        cache: Mutex::new(Cache {
            payload: None,
            last_updated: 0,
            refreshing: false,
        }),
        started_at: unix_now(),
    });

    // Prime the cache before serving so the first request is instant.
    refresh_cache(&state).await;

    let app = Router::new()
        .route("/quota", get(get_quota))
        .route("/quota/refresh", get(refresh_quota))
        .route("/health", get(health))
        .with_state(state);

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
