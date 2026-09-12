//! OpenAI Codex provider: ChatGPT subscription quota via OAuth.
//!
//! Reads the Codex CLI's own `auth.json` (default `~/.codex/auth.json`,
//! overridable with `CODEX_HOME`) and calls the same usage endpoint the CLI
//! uses: `GET {chatgpt_base_url}/wham/usage`.
//!
//! This module is strictly read-only: it never refreshes tokens and never
//! writes `auth.json`. Token refresh is owned by the Codex CLI — on a 401 we
//! report the failure and tell the user to run `codex login` again.
//!
//! Optional overrides (handy for multi-account setups and tests):
//! - `CODEX_ACCESS_TOKEN` / `CODEX_ACCOUNT_ID`
//! - `CODEX_BASE_URL` (default `https://chatgpt.com/backend-api`)

use super::{demo, ProviderError, ProviderQuota, QuotaProvider};
use crate::model::{unix_now, BillingQuota, SubscriptionQuota, UsageWindow};
use async_trait::async_trait;
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// `~/.codex/auth.json` as written by the Codex CLI.
#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    /// Present when the CLI is in API-key mode — which has no usage endpoint.
    #[serde(default)]
    #[allow(dead_code)]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
    #[serde(default)]
    #[allow(dead_code)]
    last_refresh: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    refresh_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    id_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexCredentials {
    pub access_token: String,
    pub account_id: Option<String>,
}

/// Parsed form of the `/wham/usage` response.
#[derive(Debug, PartialEq)]
pub(crate) struct CodexUsage {
    pub plan: Option<String>,
    pub windows: Vec<UsageWindow>,
    /// Undocumented semantics; treated as a USD grant when finite.
    pub credits_balance: Option<f64>,
    pub credits_unlimited: bool,
}

pub(crate) fn codex_home() -> PathBuf {
    if let Ok(home) = env::var("CODEX_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    env::var("HOME")
        .map(|h| PathBuf::from(h).join(".codex"))
        .unwrap_or_else(|_| PathBuf::from(".codex"))
}

/// Load OAuth credentials: explicit env override first, then the CLI's
/// `auth.json`. API-key mode is rejected — it has no usage endpoint.
pub(crate) fn load_codex_credentials() -> Result<CodexCredentials, ProviderError> {
    if let Some(token) = env::var("CODEX_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
    {
        let account_id = env::var("CODEX_ACCOUNT_ID")
            .ok()
            .filter(|a| !a.trim().is_empty());
        return Ok(CodexCredentials {
            access_token: token,
            account_id,
        });
    }

    let path = codex_home().join("auth.json");
    let raw = fs::read_to_string(&path).map_err(|_| {
        ProviderError::NotConfigured(
            "no Codex credentials: run `codex login` (ChatGPT flow) or set CODEX_ACCESS_TOKEN",
        )
    })?;
    let auth: CodexAuthFile = serde_json::from_str(&raw)
        .map_err(|e| ProviderError::Unexpected(format!("cannot parse {}: {e}", path.display())))?;

    match auth.tokens {
        Some(tokens) => match tokens.access_token.filter(|t| !t.trim().is_empty()) {
            Some(access_token) => Ok(CodexCredentials {
                access_token,
                account_id: tokens.account_id.filter(|a| !a.trim().is_empty()),
            }),
            None if auth.openai_api_key.is_some() => Err(ProviderError::NotConfigured(
                "Codex is in API-key mode, which has no usage endpoint: run `codex login` and pick the ChatGPT flow",
            )),
            None => Err(ProviderError::NotConfigured(
                "Codex auth.json has no OAuth access token: run `codex login`",
            )),
        },
        None if auth.openai_api_key.is_some() => Err(ProviderError::NotConfigured(
            "Codex is in API-key mode, which has no usage endpoint: run `codex login` and pick the ChatGPT flow",
        )),
        None => Err(ProviderError::NotConfigured(
            "Codex auth.json has no OAuth tokens: run `codex login`",
        )),
    }
}

/// Resolve the usage endpoint. Mirrors the CLI: `{base}/wham/usage` when the
/// base URL contains `/backend-api`, otherwise `{base}/api/codex/usage`.
pub(crate) fn usage_url() -> String {
    let base = env::var("CODEX_BASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let base = base.trim_end_matches('/');
    if base.contains("/backend-api") {
        format!("{base}/wham/usage")
    } else {
        format!("{base}/api/codex/usage")
    }
}

/// Parse a rate-limit window defensively: `reset_at` may be absent, in which
/// case `resets_at` is 0 (unknown) rather than the current time, so a client
/// can tell "we don't know" apart from "this window just reset".
fn parse_window(label: &str, value: &serde_json::Value) -> Option<UsageWindow> {
    let used = value.get("used_percent")?.as_f64()?;
    Some(UsageWindow {
        window: label.to_string(),
        limit: None,
        used,
        used_percent: Some(used),
        resets_at: value.get("reset_at").and_then(|v| v.as_u64()).unwrap_or(0),
        window_seconds: value.get("limit_window_seconds").and_then(|v| v.as_u64()),
    })
}

/// Parse the `/wham/usage` payload defensively: unknown keys are ignored,
/// malformed windows are skipped, and the known `primary_window` /
/// `secondary_window` keep their canonical "5h" / "weekly" labels. Any extra
/// model-specific windows are surfaced under their raw key.
pub(crate) fn parse_codex_usage(body: &serde_json::Value) -> CodexUsage {
    let plan = body
        .get("plan_type")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut windows = Vec::new();
    if let Some(rate_limit) = body.get("rate_limit").and_then(|v| v.as_object()) {
        for (key, label) in [("primary_window", "5h"), ("secondary_window", "weekly")] {
            if let Some(value) = rate_limit.get(key) {
                if let Some(window) = parse_window(label, value) {
                    windows.push(window);
                }
            }
        }
        for (key, value) in rate_limit {
            if key == "primary_window" || key == "secondary_window" {
                continue;
            }
            if let Some(window) = parse_window(key, value) {
                windows.push(window);
            }
        }
    }

    let credits = body.get("credits");
    CodexUsage {
        plan,
        windows,
        credits_balance: credits
            .and_then(|c| c.get("balance"))
            .and_then(|v| v.as_f64()),
        credits_unlimited: credits
            .and_then(|c| c.get("unlimited"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

pub(crate) struct CodexProvider {
    demo: bool,
}

impl CodexProvider {
    pub(crate) fn new(demo: bool) -> Self {
        Self { demo }
    }
}

#[async_trait]
impl QuotaProvider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<ProviderQuota, ProviderError> {
        let now = unix_now();
        let creds = match load_codex_credentials() {
            Ok(creds) => creds,
            Err(e) => {
                if self.demo {
                    // ChatGPT subscription: 5h + weekly windows, no billing.
                    return Ok(ProviderQuota {
                        billing: None,
                        subscription: Some(demo::subscription(
                            "pro",
                            vec![
                                demo::window("5h", 34.0, 3 * 3600, Some(5 * 3600), now),
                                demo::window(
                                    "weekly",
                                    12.0,
                                    4 * 24 * 3600,
                                    Some(7 * 24 * 3600),
                                    now,
                                ),
                            ],
                        )),
                    });
                }
                return Err(e);
            }
        };

        let mut request = client
            .get(usage_url())
            .bearer_auth(&creds.access_token)
            .header("User-Agent", "codex-cli");
        if let Some(account_id) = &creds.account_id {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        let resp = request.send().await.map_err(ProviderError::Http)?;
        match resp.status() {
            s if s.is_success() => {}
            reqwest::StatusCode::UNAUTHORIZED => {
                return Err(ProviderError::NotConfigured(
                    "Codex token expired or invalid: run `codex login` to re-authenticate (refresh is CLI-owned)",
                ))
            }
            s => {
                return Err(ProviderError::Unexpected(format!(
                    "Codex usage API returned {s}"
                )))
            }
        }

        let body: serde_json::Value = resp.json().await.map_err(ProviderError::Http)?;
        let usage = parse_codex_usage(&body);

        // Credits balance semantics are undocumented; surfaced as a USD grant
        // so the number is visible rather than silently dropped.
        let billing = match (usage.credits_unlimited, usage.credits_balance) {
            (false, Some(balance)) => Some(BillingQuota {
                total_granted: balance,
                total_used: 0.0,
                remaining_balance: balance,
                reset_timestamp: 0,
            }),
            _ => None,
        };

        Ok(ProviderQuota {
            billing,
            subscription: Some(SubscriptionQuota {
                plan: usage.plan,
                windows: usage.windows,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process-global env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn usage_fixture() -> serde_json::Value {
        serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {"used_percent": 15, "reset_at": 1735401600, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": 5, "reset_at": 1735920000, "limit_window_seconds": 604800},
                "gpt_5_3_codex": {"used_percent": 42, "reset_at": 1735401600, "limit_window_seconds": 18000}
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": 150.0}
        })
    }

    #[test]
    fn parses_full_usage_response() {
        let usage = parse_codex_usage(&usage_fixture());
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert_eq!(usage.windows.len(), 3);

        let primary = &usage.windows[0];
        assert_eq!(primary.window, "5h");
        assert_eq!(primary.used_percent, Some(15.0));
        assert_eq!(primary.resets_at, 1735401600);
        assert_eq!(primary.window_seconds, Some(18000));

        let secondary = &usage.windows[1];
        assert_eq!(secondary.window, "weekly");
        assert_eq!(secondary.used_percent, Some(5.0));
        assert_eq!(secondary.window_seconds, Some(604800));

        // Extra model-specific windows are surfaced, not dropped.
        assert_eq!(usage.windows[2].window, "gpt_5_3_codex");
        assert_eq!(usage.windows[2].used_percent, Some(42.0));

        assert_eq!(usage.credits_balance, Some(150.0));
        assert!(!usage.credits_unlimited);
    }

    #[test]
    fn skips_null_and_malformed_windows() {
        let body = serde_json::json!({
            "rate_limit": {
                "primary_window": null,
                "secondary_window": {"used_percent": "lots", "reset_at": 1735920000},
            }
        });
        let usage = parse_codex_usage(&body);
        assert!(usage.windows.is_empty());
        assert_eq!(usage.plan, None);
    }

    #[test]
    fn tolerates_missing_sections() {
        let usage = parse_codex_usage(&serde_json::json!({"plan_type": "plus"}));
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert!(usage.windows.is_empty());
        assert_eq!(usage.credits_balance, None);

        let empty = parse_codex_usage(&serde_json::json!({}));
        assert_eq!(empty.plan, None);
        assert!(empty.windows.is_empty());
    }

    #[test]
    fn missing_reset_is_unknown() {
        let body = serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 10}}
        });
        let usage = parse_codex_usage(&body);
        assert_eq!(usage.windows[0].resets_at, 0);
        assert_eq!(usage.windows[0].window_seconds, None);
    }

    #[test]
    fn usage_url_resolution() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("CODEX_BASE_URL");
        assert_eq!(usage_url(), "https://chatgpt.com/backend-api/wham/usage");

        env::set_var("CODEX_BASE_URL", "https://example.com/custom/");
        assert_eq!(usage_url(), "https://example.com/custom/api/codex/usage");

        env::set_var("CODEX_BASE_URL", "https://example.com/backend-api");
        assert_eq!(usage_url(), "https://example.com/backend-api/wham/usage");
        env::remove_var("CODEX_BASE_URL");
    }

    fn temp_codex_home(auth_json: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("codex-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("auth.json"), auth_json).unwrap();
        dir
    }

    #[test]
    fn loads_auth_from_codex_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_codex_home(
            r#"{"tokens": {"access_token": "tok123", "account_id": "acc456", "refresh_token": "ref"}}"#,
        );
        env::set_var("CODEX_HOME", &dir);
        env::remove_var("CODEX_ACCESS_TOKEN");

        let creds = load_codex_credentials().unwrap();
        assert_eq!(creds.access_token, "tok123");
        assert_eq!(creds.account_id.as_deref(), Some("acc456"));

        env::remove_var("CODEX_HOME");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_override_wins_over_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_codex_home(r#"{"tokens": {"access_token": "file-token"}}"#);
        env::set_var("CODEX_HOME", &dir);
        env::set_var("CODEX_ACCESS_TOKEN", "env-token");
        env::set_var("CODEX_ACCOUNT_ID", "env-acc");

        let creds = load_codex_credentials().unwrap();
        assert_eq!(creds.access_token, "env-token");
        assert_eq!(creds.account_id.as_deref(), Some("env-acc"));

        env::remove_var("CODEX_ACCESS_TOKEN");
        env::remove_var("CODEX_ACCOUNT_ID");
        env::remove_var("CODEX_HOME");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn api_key_mode_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_codex_home(r#"{"OPENAI_API_KEY": "sk-abc"}"#);
        env::set_var("CODEX_HOME", &dir);
        env::remove_var("CODEX_ACCESS_TOKEN");

        let err = load_codex_credentials().unwrap_err();
        assert!(matches!(err, ProviderError::NotConfigured(_)));

        env::remove_var("CODEX_HOME");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_auth_file_is_not_configured() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var(
            "CODEX_HOME",
            std::env::temp_dir().join("codex-test-definitely-missing"),
        );
        env::remove_var("CODEX_ACCESS_TOKEN");

        let err = load_codex_credentials().unwrap_err();
        assert!(matches!(err, ProviderError::NotConfigured(_)));
        env::remove_var("CODEX_HOME");
    }

    #[test]
    fn malformed_auth_file_is_unexpected() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_codex_home("not json at all");
        env::set_var("CODEX_HOME", &dir);
        env::remove_var("CODEX_ACCESS_TOKEN");

        let err = load_codex_credentials().unwrap_err();
        assert!(matches!(err, ProviderError::Unexpected(_)));
        env::remove_var("CODEX_HOME");
        fs::remove_dir_all(&dir).ok();
    }

    // ENV_LOCK is a plain std Mutex held across the `.await` below on purpose:
    // the demo branch never actually awaits internally (it returns before any
    // I/O), so this only serializes against other tests' env var mutations.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn demo_mode_returns_subscription_with_window_seconds() {
        // Matches the real path: no billing block, and (unlike Anthropic's
        // OAuth endpoint) Codex's usage API does report window_seconds.
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var(
            "CODEX_HOME",
            std::env::temp_dir().join("codex-test-demo-no-creds"),
        );
        env::remove_var("CODEX_ACCESS_TOKEN");

        let provider = CodexProvider { demo: true };
        let client = reqwest::Client::builder().build().unwrap();
        let quota = provider.fetch(&client).await.unwrap();

        assert!(quota.billing.is_none());
        let sub = quota.subscription.unwrap();
        assert_eq!(sub.plan.as_deref(), Some("pro"));
        assert_eq!(sub.windows.len(), 2);
        assert!(sub.windows.iter().all(|w| w.window_seconds.is_some()));

        env::remove_var("CODEX_HOME");
    }
}
