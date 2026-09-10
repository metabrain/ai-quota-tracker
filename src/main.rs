//! ai-quota-tracker — a hyper-lightweight, RAM-only AI quota caching daemon.
//!
//! Serves `GET /quota` over a Unix domain socket (HTTP semantics, JSON body).
//! All state lives in process memory; the socket itself sits on a tmpfs mount
//! (default `/dev/shm/ai_quota_cache.sock`) so nothing ever touches disk.
//!
//! Refreshes are lazy / pull-based: provider APIs are only contacted when a
//! local request arrives *and* the in-memory TTL has expired. Otherwise the
//! daemon idles on the socket doing nothing.
//!
//! The model tracks two kinds of quota per provider:
//!   * `billing` — API spend in USD (granted / used / remaining).
//!   * `subscription` — plan windows such as Claude's 5-hour and weekly
//!     limits, each with usage and a reset timestamp.

use async_trait::async_trait;
use axum::{extract::State, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    fmt,
    fs,
    os::unix::fs::PermissionsExt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::UnixListener,
    signal::unix::{signal, SignalKind},
    sync::Mutex,
    task::JoinSet,
};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Configuration (environment variables, all optional)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Config {
    /// Unix socket path. Must live on a tmpfs (e.g. /dev/shm) for zero disk I/O.
    socket_path: String,
    /// How long cached metrics are trusted before a refresh is triggered.
    ttl: Duration,
    /// Per-request timeout for outbound provider API calls.
    fetch_timeout: Duration,
    /// When true and no API key/token is set, providers return synthetic demo
    /// metrics so the daemon is useful out of the box. Set to 0 with real keys.
    demo_mode: bool,
}

impl Config {
    fn from_env() -> Self {
        let socket_path =
            env::var("SOCKET_PATH").unwrap_or_else(|_| "/dev/shm/ai_quota_cache.sock".into());
        let ttl_secs: u64 = env::var("TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let fetch_timeout_secs: u64 = env::var("FETCH_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let demo_mode = env::var("AI_QUOTA_DEMO")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        Self {
            socket_path,
            ttl: Duration::from_secs(ttl_secs),
            fetch_timeout: Duration::from_secs(fetch_timeout_secs),
            demo_mode,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// API spend in USD (e.g. OpenAI platform billing).
#[derive(Debug, Clone, Serialize, Default)]
struct BillingQuota {
    total_granted: f64,
    total_used: f64,
    remaining_balance: f64,
    reset_timestamp: u64,
}

/// One subscription usage window, e.g. Claude's "5h" or "weekly" limits.
#[derive(Debug, Clone, Serialize)]
struct UsageWindow {
    /// Window label: "5h", "weekly", ...
    window: String,
    /// Absolute cap in window units, when known. Null = unknown / unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<f64>,
    /// Consumed amount in window units.
    used: f64,
    /// 0–100 when the provider reports a percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    used_percent: Option<f64>,
    /// Unix timestamp when this window resets.
    resets_at: u64,
}

/// Subscription-plan quota: a set of rolling usage windows.
#[derive(Debug, Clone, Serialize, Default)]
struct SubscriptionQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    windows: Vec<UsageWindow>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ProviderQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    billing: Option<BillingQuota>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<SubscriptionQuota>,
}

#[derive(Debug, Clone, Serialize)]
struct AiQuotaPayload {
    timestamp: u64,
    ttl_seconds: u64,
    /// Per-provider failures from the last refresh, if any. Providers that
    /// failed keep serving their last-known-good metrics from `providers`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    errors: HashMap<String, String>,
    providers: HashMap<String, ProviderQuota>,
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ProviderError {
    NotConfigured(&'static str),
    Http(reqwest::Error),
    Unexpected(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NotConfigured(msg) => write!(f, "not configured: {msg}"),
            ProviderError::Http(e) => write!(f, "http error: {e}"),
            ProviderError::Unexpected(msg) => write!(f, "unexpected response: {msg}"),
        }
    }
}

#[async_trait]
trait QuotaProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> Result<ProviderQuota, ProviderError>;
}

/// Synthetic metrics used in demo mode (AI_QUOTA_DEMO=1 with no key/token set).
fn demo_quota(now: u64) -> ProviderQuota {
    ProviderQuota {
        billing: Some(BillingQuota {
            total_granted: 120.00,
            total_used: 45.22,
            remaining_balance: 74.78,
            reset_timestamp: now + 86400 * 15,
        }),
        subscription: Some(SubscriptionQuota {
            plan: Some("demo".to_string()),
            windows: vec![
                UsageWindow {
                    window: "5h".to_string(),
                    limit: None,
                    used: 23.5,
                    used_percent: Some(23.5),
                    resets_at: now + 7_200,
                },
                UsageWindow {
                    window: "weekly".to_string(),
                    limit: None,
                    used: 41.2,
                    used_percent: Some(41.2),
                    resets_at: now + 86400 * 3,
                },
            ],
        }),
    }
}

// --- OpenAI ----------------------------------------------------------------

struct OpenAiProvider {
    api_key: Option<String>,
    /// Monthly grant in USD, from OPENAI_GRANTED_USD (the usage API reports
    /// spend; the grant itself is configured by you).
    granted_usd: f64,
    demo: bool,
}

impl OpenAiProvider {
    fn new(demo: bool) -> Self {
        let api_key = env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
        let granted_usd: f64 = env::var("OPENAI_GRANTED_USD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        Self {
            api_key,
            granted_usd,
            demo,
        }
    }
}

#[async_trait]
impl QuotaProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
        let now = unix_now();
        let Some(key) = &self.api_key else {
            if self.demo {
                return Ok(demo_quota(now));
            }
            return Err(ProviderError::NotConfigured(
                "set OPENAI_API_KEY or enable AI_QUOTA_DEMO=1",
            ));
        };

        // Organization costs for the trailing 30 days (requires an admin key).
        let resp = client
            .get("https://api.openai.com/v1/organization/costs")
            .query(&[
                ("start_time", (now.saturating_sub(86400 * 30)).to_string()),
                ("limit", "100".to_string()),
            ])
            .bearer_auth(key)
            .send()
            .await
            .map_err(ProviderError::Http)?;

        if !resp.status().is_success() {
            return Err(ProviderError::Unexpected(format!(
                "OpenAI costs API returned {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp.json().await.map_err(ProviderError::Http)?;
        let mut used = 0.0f64;
        if let Some(entries) = body.get("data").and_then(|d| d.as_array()) {
            for entry in entries {
                if let Some(v) = entry.pointer("/amount/value").and_then(|v| v.as_f64()) {
                    used += v;
                }
            }
        }
        Ok(ProviderQuota {
            billing: Some(BillingQuota {
                total_granted: self.granted_usd,
                total_used: used,
                remaining_balance: (self.granted_usd - used).max(0.0),
                reset_timestamp: now + 86400 * 30,
            }),
            // ChatGPT subscription tiers expose no public usage API.
            subscription: None,
        })
    }
}

// --- Anthropic (Claude subscription windows) ---------------------------------

/// Claude Pro / Max subscriptions report 5-hour and 7-day usage through the
/// OAuth usage endpoint (the same one Claude Code uses).
struct AnthropicProvider {
    oauth_token: Option<String>,
    demo: bool,
}

impl AnthropicProvider {
    fn new(demo: bool) -> Self {
        let oauth_token = env::var("ANTHROPIC_OAUTH_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(read_claude_oauth_token);
        Self { oauth_token, demo }
    }
}

/// Pick up the token Claude Code stores after `claude login`, so the daemon
/// works with zero extra configuration on a dev machine.
fn read_claude_oauth_token() -> Option<String> {
    let home = env::var("HOME").ok()?;
    let raw = fs::read_to_string(format!("{home}/.claude/.credentials.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.pointer("/claudeAiOauth/accessToken")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

#[derive(Debug, Deserialize)]
struct OauthUsageWindow {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OauthUsage {
    five_hour: Option<OauthUsageWindow>,
    seven_day: Option<OauthUsageWindow>,
}

fn to_usage_window(label: &str, w: OauthUsageWindow, now: u64) -> UsageWindow {
    let resets_at = w
        .resets_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(now);
    UsageWindow {
        window: label.to_string(),
        limit: None, // Anthropic reports percentage utilization, not absolute caps
        used: w.utilization,
        used_percent: Some(w.utilization),
        resets_at,
    }
}

#[async_trait]
impl QuotaProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
        let now = unix_now();
        let Some(token) = &self.oauth_token else {
            if self.demo {
                return Ok(demo_quota(now));
            }
            return Err(ProviderError::NotConfigured(
                "set ANTHROPIC_OAUTH_TOKEN (or sign in via Claude Code) or enable AI_QUOTA_DEMO=1",
            ));
        };

        let resp = client
            .get("https://api.anthropic.com/api/oauth/usage")
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .map_err(ProviderError::Http)?;

        if !resp.status().is_success() {
            return Err(ProviderError::Unexpected(format!(
                "Anthropic usage API returned {}",
                resp.status()
            )));
        }
        let usage: OauthUsage = resp.json().await.map_err(ProviderError::Http)?;
        let mut windows = Vec::new();
        if let Some(w) = usage.five_hour {
            windows.push(to_usage_window("5h", w, now));
        }
        if let Some(w) = usage.seven_day {
            windows.push(to_usage_window("weekly", w, now));
        }
        Ok(ProviderQuota {
            billing: None,
            subscription: Some(SubscriptionQuota {
                plan: Some("claude".to_string()),
                windows,
            }),
        })
    }
}

// ---------------------------------------------------------------------------
// In-memory cache
// ---------------------------------------------------------------------------

struct Cache {
    payload: Option<AiQuotaPayload>,
    last_updated: u64,
    /// Guards against thundering-herd refreshes: only one runs at a time,
    /// concurrent requests serve the existing (possibly stale) payload.
    refreshing: bool,
}

struct AppState {
    config: Config,
    client: reqwest::Client,
    providers: Vec<Arc<dyn QuotaProvider>>,
    cache: Mutex<Cache>,
    started_at: u64,
}

/// Pull fresh metrics from every provider concurrently and swap the cache.
/// Never clears last-known-good metrics on partial failure.
async fn refresh_cache(state: &Arc<AppState>) {
    {
        let mut cache = state.cache.lock().await;
        if cache.refreshing {
            info!("refresh already in flight; serving existing cache");
            return;
        }
        cache.refreshing = true;
    }

    let now = unix_now();
    let mut set = JoinSet::new();
    for provider in &state.providers {
        let provider = Arc::clone(provider);
        let client = state.client.clone();
        set.spawn(async move { (provider.name().to_string(), provider.fetch(&client).await) });
    }

    let mut providers = HashMap::new();
    let mut errors = HashMap::new();
    while let Some(done) = set.join_next().await {
        match done {
            Ok((name, Ok(quota))) => {
                providers.insert(name, quota);
            }
            Ok((name, Err(e))) => {
                warn!(provider = %name, error = %e, "provider refresh failed");
                errors.insert(name, e.to_string());
            }
            Err(e) => error!(error = %e, "provider task failed"),
        }
    }

    // Keep last-known-good metrics for any provider that failed this round.
    let mut cache = state.cache.lock().await;
    if let Some(previous) = cache.payload.take() {
        for (name, quota) in previous.providers {
            providers.entry(name).or_insert(quota);
        }
    }

    cache.payload = Some(AiQuotaPayload {
        timestamp: now,
        ttl_seconds: state.config.ttl.as_secs(),
        errors,
        providers,
    });
    cache.last_updated = now;
    cache.refreshing = false;
    info!("cache refreshed");
}

// ---------------------------------------------------------------------------
// HTTP handlers (served over the Unix socket)
// ---------------------------------------------------------------------------

async fn get_quota(State(state): State<Arc<AppState>>) -> Json<AiQuotaPayload> {
    let now = unix_now();
    let stale = {
        let cache = state.cache.lock().await;
        match &cache.payload {
            None => true,
            Some(_) => now.saturating_sub(cache.last_updated) >= state.config.ttl.as_secs(),
        }
    };
    if stale {
        info!("cache miss/expired — refreshing from providers");
        refresh_cache(&state).await;
    }
    let cache = state.cache.lock().await;
    // refresh_cache always installs a payload (possibly with empty maps), so
    // this is unreachable in practice; the expect documents the invariant.
    Json(cache.payload.clone().expect("cache populated by refresh"))
}

async fn force_refresh(State(state): State<Arc<AppState>>) -> Json<AiQuotaPayload> {
    info!("forced refresh requested");
    refresh_cache(&state).await;
    let cache = state.cache.lock().await;
    Json(cache.payload.clone().expect("cache populated by refresh"))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "uptime_seconds": unix_now().saturating_sub(state.started_at),
        "socket": state.config.socket_path,
        "ttl_seconds": state.config.ttl.as_secs(),
        "demo_mode": state.config.demo_mode,
    }))
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ai_quota_tracker=info".parse().expect("valid directive")),
        )
        .init();

    let config = Config::from_env();
    info!(
        socket = %config.socket_path,
        ttl_secs = config.ttl.as_secs(),
        demo_mode = config.demo_mode,
        "starting ai-quota-tracker"
    );

    // Drop any socket left behind by an unclean shutdown so bind() can't fail.
    let _ = fs::remove_file(&config.socket_path);

    let listener = match UnixListener::bind(&config.socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("FATAL: cannot bind {}: {e}", config.socket_path);
            std::process::exit(1);
        }
    };

    // Process-level authorization: only this user may connect.
    if let Err(e) = fs::set_permissions(
        &config.socket_path,
        fs::Permissions::from_mode(0o600),
    ) {
        eprintln!("FATAL: cannot chmod 600 {}: {e}", config.socket_path);
        let _ = fs::remove_file(&config.socket_path);
        std::process::exit(1);
    }

    let client = reqwest::Client::builder()
        .timeout(config.fetch_timeout)
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!("ai-quota-tracker/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client builds");

    let state = Arc::new(AppState {
        config: config.clone(),
        client,
        providers: vec![
            Arc::new(OpenAiProvider::new(config.demo_mode)),
            Arc::new(AnthropicProvider::new(config.demo_mode)),
        ],
        cache: Mutex::new(Cache {
            payload: None,
            last_updated: 0,
            refreshing: false,
        }),
        started_at: unix_now(),
    });

    let app = Router::new()
        .route("/quota", get(get_quota))
        .route("/quota/refresh", get(force_refresh))
        .route("/health", get(health))
        .with_state(state);

    info!("RAM-only daemon listening on {}", config.socket_path);

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
