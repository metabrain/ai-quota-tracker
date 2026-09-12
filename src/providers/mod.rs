//! Provider trait, error type, and shared demo fixtures.
//!
//! A provider is one upstream account: OpenAI platform billing, Codex ChatGPT
//! subscription, Claude Code OAuth, ... Each one knows how to fetch its own
//! quota and map it onto the shared [`ProviderQuota`] model. Providers never
//! share credentials and never write to each other's files.

pub(crate) mod anthropic;
pub(crate) mod codex;
pub(crate) mod muse_code;
pub(crate) mod openai;

use crate::model::ProviderQuota;
use async_trait::async_trait;
use std::fmt;

/// What can go wrong while fetching one provider's quota.
#[derive(Debug)]
pub(crate) enum ProviderError {
    /// Missing credentials / unsupported auth mode. Carries a human hint.
    NotConfigured(&'static str),
    /// Configured, but the provider exposes no quota signal we can report.
    /// Carries a human-readable explanation (dynamic: may name what we saw).
    Unsupported(String),
    /// HTTP transport or JSON decode failure.
    Http(reqwest::Error),
    /// Anything else, with context.
    Unexpected(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NotConfigured(hint) => write!(f, "not configured: {hint}"),
            ProviderError::Unsupported(detail) => write!(f, "unsupported: {detail}"),
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

/// Stand-in data for demo mode (no credentials, `QUOTA_DEMO_MODE` on).
///
/// Each provider builds its demo payload from these so the fake output has the
/// same *shape* it would return for real — billing-only for OpenAI,
/// subscription windows for Codex/Anthropic — rather than every provider
/// emitting one identical blob. Providers with no reportable quota at all
/// (Muse) return their real `Unsupported`/`NotConfigured` error instead.
pub(crate) mod demo {
    use crate::model::{BillingQuota, SubscriptionQuota, UsageWindow};

    pub(crate) fn billing(_now: u64) -> BillingQuota {
        BillingQuota {
            total_granted: 120.0,
            total_used: 37.42,
            remaining_balance: 82.58,
            // Matches the real OpenAI provider: a rolling 30-day window has
            // no discrete reset, so 0 = unknown rather than a fabricated
            // `now + 30d`.
            reset_timestamp: 0,
        }
    }

    pub(crate) fn window(
        label: &str,
        used_percent: f64,
        resets_in: u64,
        window_seconds: Option<u64>,
        now: u64,
    ) -> UsageWindow {
        UsageWindow {
            window: label.to_string(),
            limit: None,
            used: used_percent,
            used_percent: Some(used_percent),
            resets_at: now + resets_in,
            window_seconds,
        }
    }

    pub(crate) fn subscription(plan: &str, windows: Vec<UsageWindow>) -> SubscriptionQuota {
        SubscriptionQuota {
            plan: Some(plan.to_string()),
            windows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::demo;

    #[test]
    fn demo_billing_has_no_discrete_reset() {
        // Regression test for #33: demo::billing() is OpenAI's demo-mode
        // stand-in and must mirror the real provider's convention — a
        // rolling 30-day window has no reset, so 0 (unknown), never a
        // fabricated `now + 30d`.
        let billing = demo::billing(1_700_000_000);
        assert_eq!(billing.reset_timestamp, 0);
    }
}
