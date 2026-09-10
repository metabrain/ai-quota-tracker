//! Anthropic provider: Claude Code subscription quota via OAuth.
//!
//! Calls `GET https://api.anthropic.com/api/oauth/usage` with the OAuth
//! access token the Claude Code CLI stores in `~/.claude/.credentials.json`
//! (or `ANTHROPIC_OAUTH_TOKEN`). Maps the 5h / 7d utilization windows onto
//! the shared model. Read-only: never touches the CLI's credential file.

use super::{demo_quota, ProviderError, ProviderQuota, QuotaProvider};
use crate::model::{unix_now, SubscriptionQuota, UsageWindow};
use async_trait::async_trait;
use serde::Deserialize;
use std::env;

pub(crate) struct AnthropicProvider {
    oauth_token: Option<String>,
    demo: bool,
}

impl AnthropicProvider {
    pub(crate) fn new(demo: bool) -> Self {
        let oauth_token = env::var("ANTHROPIC_OAUTH_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(load_claude_oauth_token);
        Self { oauth_token, demo }
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeCredentialsFile {
    #[serde(default)]
    claude_ai_oauth: Option<ClaudeAiOauth>,
}

#[derive(Debug, Deserialize)]
struct ClaudeAiOauth {
    #[serde(default)]
    access_token: Option<String>,
}

/// Read the OAuth token the Claude Code CLI manages. Read-only.
fn load_claude_oauth_token() -> Option<String> {
    let home = env::var("HOME").ok()?;
    let path = format!("{home}/.claude/.credentials.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let creds: ClaudeCredentialsFile = serde_json::from_str(&raw).ok()?;
    creds
        .claude_ai_oauth?
        .access_token
        .filter(|t| !t.trim().is_empty())
}

#[derive(Debug, Deserialize)]
struct OauthUsageWindow {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OauthUsageResponse {
    #[serde(default)]
    five_hour: Option<OauthUsageWindow>,
    #[serde(default)]
    seven_day: Option<OauthUsageWindow>,
}

fn parse_resets_at(s: Option<&str>, now: u64) -> u64 {
    s.and_then(|text| {
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|dt| dt.timestamp().max(0) as u64)
    })
    .unwrap_or(now)
}

fn to_usage_window(label: &str, w: &OauthUsageWindow, now: u64) -> Option<UsageWindow> {
    let used = w.utilization?;
    Some(UsageWindow {
        window: label.to_string(),
        limit: None,
        used,
        used_percent: Some(used),
        resets_at: parse_resets_at(w.resets_at.as_deref(), now),
        window_seconds: None,
    })
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
                "set ANTHROPIC_OAUTH_TOKEN or run `claude login`",
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
        let usage: OauthUsageResponse = resp.json().await.map_err(ProviderError::Http)?;

        let mut windows = Vec::new();
        if let Some(w) = &usage.five_hour {
            if let Some(win) = to_usage_window("5h", w, now) {
                windows.push(win);
            }
        }
        if let Some(w) = &usage.seven_day {
            if let Some(win) = to_usage_window("weekly", w, now) {
                windows.push(win);
            }
        }

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

    #[test]
    fn window_mapping() {
        let w = OauthUsageWindow {
            utilization: Some(33.0),
            resets_at: Some("2026-04-11T07:00:00+00:00".to_string()),
        };
        let win = to_usage_window("5h", &w, 1_000_000).unwrap();
        assert_eq!(win.window, "5h");
        assert_eq!(win.used_percent, Some(33.0));
        assert!(win.resets_at > 1_000_000);
    }

    #[test]
    fn missing_utilization_is_skipped() {
        let w = OauthUsageWindow {
            utilization: None,
            resets_at: None,
        };
        assert!(to_usage_window("5h", &w, 1_000_000).is_none());
    }

    #[test]
    fn bad_timestamp_falls_back_to_now() {
        assert_eq!(parse_resets_at(Some("not-a-time"), 42), 42);
        assert_eq!(parse_resets_at(None, 42), 42);
    }
}
