//! OpenAI provider: platform API billing in USD.
//!
//! Uses the (undocumented but long-stable) organization costs endpoint with a
//! standard `OPENAI_API_KEY`. There is no public ChatGPT subscription quota
//! endpoint — Codex/ChatGPT subscription limits live in [`super::codex`].

use super::{demo_quota, ProviderError, ProviderQuota, QuotaProvider};
use crate::model::{unix_now, BillingQuota};
use async_trait::async_trait;
use std::env;

/// Sum every `data[].amount.value` in an organization-costs response.
/// Unknown shapes contribute 0 instead of failing the whole provider.
pub(crate) fn sum_costs(body: &serde_json::Value) -> f64 {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("amount")?.get("value")?.as_f64())
                .sum()
        })
        .unwrap_or(0.0)
}

pub(crate) struct OpenAiProvider {
    api_key: Option<String>,
    /// Monthly grant in USD, from OPENAI_GRANTED_USD (the usage API reports
    /// spend; the grant itself is configured by you).
    granted_usd: f64,
    demo: bool,
}

impl OpenAiProvider {
    pub(crate) fn new(demo: bool) -> Self {
        let api_key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty());
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
        let Some(api_key) = &self.api_key else {
            if self.demo {
                return Ok(demo_quota(now));
            }
            return Err(ProviderError::NotConfigured("set OPENAI_API_KEY"));
        };

        // Organization costs for the trailing 30 days (requires an admin key).
        let resp = client
            .get("https://api.openai.com/v1/organization/costs")
            .query(&[
                ("start_time", now.saturating_sub(86400 * 30).to_string()),
                ("limit", "100".to_string()),
            ])
            .bearer_auth(api_key)
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
        let used = sum_costs(&body);
        Ok(ProviderQuota {
            billing: Some(BillingQuota {
                total_granted: self.granted_usd,
                total_used: used,
                remaining_balance: (self.granted_usd - used).max(0.0),
                reset_timestamp: now + 86400 * 30,
            }),
            // ChatGPT subscription tiers expose no public usage API;
            // Codex/ChatGPT subscription limits live in the `codex` provider.
            subscription: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_all_cost_entries() {
        let body = serde_json::json!({
            "data": [
                {"amount": {"value": 1.5, "currency": "usd"}},
                {"amount": {"value": 2.25, "currency": "usd"}},
                {"amount": {"value": 0}},
            ]
        });
        let total = sum_costs(&body);
        assert!((total - 3.75).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn ignores_malformed_entries_and_shapes() {
        let body = serde_json::json!({
            "data": [
                {"amount": {"value": "not-a-number"}},
                {"amount": {}},
                {"nope": true},
                "a-string",
            ]
        });
        assert_eq!(sum_costs(&body), 0.0);
        assert_eq!(sum_costs(&serde_json::json!({})), 0.0);
        assert_eq!(sum_costs(&serde_json::json!({"data": "nope"})), 0.0);
    }
}
