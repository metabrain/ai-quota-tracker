//! Provider trait, error type, and shared demo fixtures.
//!
//! A provider is one upstream account: OpenAI platform billing, Codex ChatGPT
//! subscription, Claude Code OAuth, ... Each one knows how to fetch its own
//! quota and map it onto the shared [`ProviderQuota`] model. Providers never
//! share credentials and never write to each other's files.

pub(crate) mod anthropic;
pub(crate) mod codex;
pub(crate) mod openai;

use crate::model::{BillingQuota, ProviderQuota, SubscriptionQuota, UsageWindow};
use async_trait::async_trait;
use std::fmt;

/// What can go wrong while fetching one provider's quota.
#[derive(Debug)]
pub(crate) enum ProviderError {
    /// Missing credentials / unsupported auth mode. Carries a human hint.
    NotConfigured(&'static str),
    /// HTTP transport or JSON decode failure.
    Http(reqwest::Error),
    /// Anything else, with context.
    Unexpected(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NotConfigured(hint) => write!(f, "not configured: {hint}"),
            ProviderError::Http(e) => write!(f, "http error: {e}"),
            ProviderError::Unexpected(msg) => write!(f, "unexpected error: {msg}"),
        }
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(e: reqwest::Error) -> Self {
        ProviderError::Http(e)
    }
}

/// One quota source. Must be `Send + Sync` so refresh can run providers in
/// parallel on a shared client.
#[async_trait]
pub(crate) trait QuotaProvider: Send + Sync {
    /// Stable key used in the JSON response, e.g. `"codex"`.
    fn name(&self) -> &'static str;
    async fn fetch(&self, client: &reqwest::Client) -> Result<ProviderQuota, ProviderError>;
}

/// Synthetic quota used when no credentials exist and demo mode is on.
pub(crate) fn demo_quota(now: u64) -> ProviderQuota {
    ProviderQuota {
        billing: Some(BillingQuota {
            total_granted: 120.0,
            total_used: 37.42,
            remaining_balance: 82.58,
            reset_timestamp: now + 30 * 24 * 3600,
        }),
        subscription: Some(SubscriptionQuota {
            plan: Some("demo".to_string()),
            windows: vec![
                UsageWindow {
                    window: "5h".to_string(),
                    limit: None,
                    used: 34.0,
                    used_percent: Some(34.0),
                    resets_at: now + 3 * 3600,
                    window_seconds: Some(5 * 3600),
                },
                UsageWindow {
                    window: "weekly".to_string(),
                    limit: None,
                    used: 12.0,
                    used_percent: Some(12.0),
                    resets_at: now + 4 * 24 * 3600,
                    window_seconds: Some(7 * 24 * 3600),
                },
            ],
        }),
    }
}
