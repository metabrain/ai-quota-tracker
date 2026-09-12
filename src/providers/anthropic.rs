//! Anthropic provider: Claude Code subscription quota via OAuth.
//!
//! Calls `GET https://api.anthropic.com/api/oauth/usage` with the OAuth
//! access token the Claude Code CLI stores in `~/.claude/.credentials.json`
//! (or `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_OAUTH_TOKEN`). Maps the 5h / 7d
//! utilization windows onto the shared model. Read-only: never touches the
//! CLI's credential file and never refreshes tokens.

use super::{demo, ProviderError, ProviderQuota, QuotaProvider};
use crate::model::{unix_now, SubscriptionQuota, UsageWindow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::{env, fs, path::PathBuf};

/// The usage endpoint rate-limits non-Claude-Code user agents aggressively
/// (persistent 429s reported by community tooling), so we identify as the CLI.
const CLAUDE_CODE_USER_AGENT: &str = "claude-code/2.1.0";

/// Known usage buckets, in display order. Anything else in the payload is
/// surfaced generically rather than dropped (the API grows new buckets).
const KNOWN_WINDOWS: &[(&str, &str)] = &[
    ("five_hour", "5h"),
    ("seven_day", "weekly"),
    ("seven_day_sonnet", "weekly (sonnet)"),
    ("seven_day_opus", "weekly (opus)"),
];

pub(crate) struct AnthropicProvider {
    oauth_token: Option<String>,
    demo: bool,
}

impl AnthropicProvider {
    pub(crate) fn new(demo: bool) -> Self {
        Self {
            oauth_token: load_oauth_token(),
            demo,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCredentialsFile {
    #[serde(default)]
    claude_ai_oauth: Option<ClaudeAiOauth>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAiOauth {
    #[serde(default)]
    access_token: Option<String>,
}

fn token_from_env(vars: &[&str]) -> Option<String> {
    vars.iter()
        .find_map(|var| env::var(var).ok().filter(|t| !t.trim().is_empty()))
}

fn credentials_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".claude/.credentials.json"))
}

/// Token precedence: explicit env (`CLAUDE_CODE_OAUTH_TOKEN`, then the legacy
/// `ANTHROPIC_OAUTH_TOKEN`) wins over the CLI-managed credential file.
pub(crate) fn load_oauth_token() -> Option<String> {
    token_from_env(&["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_OAUTH_TOKEN"]).or_else(|| {
        let raw = fs::read_to_string(credentials_path()?).ok()?;
        let creds: ClaudeCredentialsFile = serde_json::from_str(&raw).ok()?;
        creds
            .claude_ai_oauth?
            .access_token
            .filter(|t| !t.trim().is_empty())
    })
}

/// Parse an RFC3339 reset timestamp. A missing or unparsable value becomes 0 —
/// the "unknown" sentinel — so a client can tell "we don't know" apart from a
/// window that just reset.
fn parse_resets_at(s: Option<&str>) -> u64 {
    s.and_then(|text| {
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|dt| dt.timestamp().max(0) as u64)
    })
    .unwrap_or(0)
}

fn parse_window(label: &str, value: &Value) -> Option<UsageWindow> {
    let used = value.get("utilization")?.as_f64()?;
    Some(UsageWindow {
        window: label.to_string(),
        limit: None,
        used,
        used_percent: Some(used),
        resets_at: parse_resets_at(value.get("resets_at").and_then(|v| v.as_str())),
        window_seconds: None,
    })
}

/// Parse the OAuth usage payload defensively: known buckets first in a stable
/// order, then any extra buckets under their raw key. `null` buckets and
/// malformed entries are skipped; `extra_usage` is not a window.
pub(crate) fn parse_oauth_usage(body: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    let Some(obj) = body.as_object() else {
        return windows;
    };
    for (key, label) in KNOWN_WINDOWS {
        if let Some(value) = obj.get(*key) {
            if let Some(window) = parse_window(label, value) {
                windows.push(window);
            }
        }
    }
    for (key, value) in obj {
        if key == "extra_usage" || KNOWN_WINDOWS.iter().any(|(k, _)| k == key) {
            continue;
        }
        if let Some(window) = parse_window(key, value) {
            windows.push(window);
        }
    }
    windows
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
                // Claude Code subscription: 5h + weekly utilization, no billing,
                // no window_seconds (the OAuth endpoint doesn't report it).
                return Ok(ProviderQuota {
                    billing: None,
                    subscription: Some(demo::subscription(
                        "claude-code",
                        vec![
                            demo::window("5h", 23.5, 3 * 3600, None, now),
                            demo::window("weekly", 41.2, 4 * 24 * 3600, None, now),
                        ],
                    )),
                });
            }
            return Err(ProviderError::NotConfigured(
                "set CLAUDE_CODE_OAUTH_TOKEN or run `claude login`",
            ));
        };

        let resp = client
            .get("https://api.anthropic.com/api/oauth/usage")
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .send()
            .await
            .map_err(ProviderError::Http)?;
        match resp.status() {
            s if s.is_success() => {}
            reqwest::StatusCode::UNAUTHORIZED => {
                // Access tokens expire ~hourly; the CLI refreshes them when it
                // runs. We never redeem the refresh token ourselves.
                return Err(ProviderError::NotConfigured(
                    "Anthropic OAuth token expired or invalid: run `claude` (or `claude update`) to refresh it",
                ));
            }
            s => {
                return Err(ProviderError::Unexpected(format!(
                    "Anthropic usage API returned {s}"
                )))
            }
        }
        let body: Value = resp.json().await.map_err(ProviderError::Http)?;
        let windows = parse_oauth_usage(&body);

        Ok(ProviderQuota {
            billing: None,
            subscription: Some(SubscriptionQuota {
                plan: Some("claude-code".to_string()),
                windows,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process-global env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn usage_fixture() -> Value {
        serde_json::json!({
            "five_hour": {"utilization": 33.0, "resets_at": "2026-04-11T07:00:00.528743+00:00"},
            "seven_day": {"utilization": 13.0, "resets_at": "2026-04-17T00:59:59.951719+00:00"},
            "seven_day_opus": null,
            "seven_day_sonnet": {"utilization": 1.0, "resets_at": "2026-04-16T03:00:00.951719+00:00"},
            "seven_day_cowork": {"utilization": 7.5, "resets_at": null},
            "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null}
        })
    }

    #[test]
    fn parses_all_windows_in_stable_order() {
        let windows = parse_oauth_usage(&usage_fixture());
        let labels: Vec<&str> = windows.iter().map(|w| w.window.as_str()).collect();
        assert_eq!(
            labels,
            vec!["5h", "weekly", "weekly (sonnet)", "seven_day_cowork"]
        );
        assert_eq!(windows[0].used_percent, Some(33.0));
        assert!(windows[0].resets_at > 0);
        // null resets_at is unknown, not "now"
        assert_eq!(windows[3].resets_at, 0);
    }

    #[test]
    fn skips_null_and_malformed_buckets() {
        let body = serde_json::json!({
            "five_hour": null,
            "seven_day": {"utilization": "lots"},
            "extra_usage": {"is_enabled": true},
        });
        assert!(parse_oauth_usage(&body).is_empty());
        assert!(parse_oauth_usage(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn bad_timestamp_is_unknown() {
        assert_eq!(parse_resets_at(Some("not-a-time")), 0);
        assert_eq!(parse_resets_at(None), 0);
    }

    #[test]
    fn env_token_wins_over_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("claude-test-{}", std::process::id()));
        let claude_dir = home.join(".claude");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth": {"accessToken": "file-token"}}"#,
        )
        .unwrap();

        env::set_var("HOME", &home);
        env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        env::remove_var("ANTHROPIC_OAUTH_TOKEN");
        assert_eq!(load_oauth_token().as_deref(), Some("file-token"));

        env::set_var("ANTHROPIC_OAUTH_TOKEN", "legacy-env-token");
        assert_eq!(load_oauth_token().as_deref(), Some("legacy-env-token"));

        env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "new-env-token");
        assert_eq!(load_oauth_token().as_deref(), Some("new-env-token"));

        env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        env::remove_var("ANTHROPIC_OAUTH_TOKEN");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_everything_is_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var(
            "HOME",
            std::env::temp_dir().join("claude-test-definitely-missing"),
        );
        env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        env::remove_var("ANTHROPIC_OAUTH_TOKEN");
        assert_eq!(load_oauth_token(), None);
    }

    #[tokio::test]
    async fn demo_mode_returns_subscription_without_window_seconds() {
        // Matches the real path: no billing block, and (unlike Codex's usage
        // API) the OAuth usage endpoint never reports window_seconds.
        let provider = AnthropicProvider {
            oauth_token: None,
            demo: true,
        };
        let client = reqwest::Client::builder().build().unwrap();
        let quota = provider.fetch(&client).await.unwrap();

        assert!(quota.billing.is_none());
        let sub = quota.subscription.unwrap();
        assert_eq!(sub.plan.as_deref(), Some("claude-code"));
        // Ordered labels, matching the real OAuth endpoint's KNOWN_WINDOWS
        // order (see parse_oauth_usage).
        let labels: Vec<&str> = sub.windows.iter().map(|w| w.window.as_str()).collect();
        assert_eq!(labels, vec!["5h", "weekly"]);
        assert!(sub.windows.iter().all(|w| w.window_seconds.is_none()));
    }
}
