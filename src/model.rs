//! Shared data model: the JSON shape served on `/quota`.
//!
//! Each provider reports two independent quota dimensions:
//!
//! - `billing`: API spend in USD (granted / used / remaining / reset).
//! - `subscription`: the seat/subscription rate limits most coding assistants
//!   actually hit — the familiar "5h" and "weekly" rolling windows, plus any
//!   extra windows a provider exposes (e.g. per-model windows).

use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// API spend in USD (e.g. OpenAI platform billing, Codex credits).
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub(crate) struct BillingQuota {
    pub total_granted: f64,
    pub total_used: f64,
    pub remaining_balance: f64,
    /// Unix timestamp of the next reset; 0 = unknown / no scheduled reset.
    pub reset_timestamp: u64,
}

/// One subscription usage window, e.g. Claude's "5h" or "weekly" limits.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct UsageWindow {
    /// Window label: "5h", "weekly", "weekly (sonnet)", ...
    pub window: String,
    /// Absolute cap in window units, when known. Absent = unknown / unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// Consumed amount in window units.
    pub used: f64,
    /// 0–100 when the provider reports a percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Unix timestamp when this window resets.
    pub resets_at: u64,
    /// Window length in seconds, when the provider reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u64>,
}

/// Subscription seat: plan label plus its rate-limit windows.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub(crate) struct SubscriptionQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
}

/// Everything we know about one provider's quota.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub(crate) struct ProviderQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<BillingQuota>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionQuota>,
}

/// Top-level `/quota` response.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AiQuotaPayload {
    pub updated_at: u64,
    pub providers: std::collections::HashMap<String, ProviderQuota>,
    #[serde(default)]
    pub errors: std::collections::HashMap<String, String>,
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
