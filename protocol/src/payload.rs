//! Frame payload bodies (spec §2.4).

use crate::ids::{Provider, TokenType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `http_request` payload downstreamed to a publisher.
///
/// `model` and `exp` complete the quota_sig verification inputs on the client —
/// the signature body is `{task_id|model|budget|exp}`, so without them the
/// signature cannot be checked at all. Both carry serde defaults so a frame
/// from an older gateway still parses; it just cannot be verified (see the
/// client's `executor::execute`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequestPayload {
    pub id: String,
    pub task_id: String,
    pub quota_sig: String,
    pub upstream: UpstreamPayload,
    pub budget_tokens: i64,
    pub stream: bool,
    /// Model the quota was granted for (part of the quota_sig body).
    #[serde(default)]
    pub model: String,
    /// Quota signature expiry, unix seconds (part of the quota_sig body).
    #[serde(default)]
    pub exp: i64,
}

/// The upstream call a publisher is asked to make. Authorization is injected by
/// the publisher from its own secret store and is never present here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamPayload {
    pub provider: String,
    pub method: String,
    pub url: String,
    pub headers: serde_json::Map<String, Value>,
    pub body_b64: String,
}

/// One lane in a `supply.declare` / `supply.update` snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplyItem {
    pub model: String,
    pub provider: Provider,
    /// Estimated serviceable tokens in the current rate window.
    pub window_remaining: i64,
    /// Minimum acceptable price, micro-USDT per 1k tokens.
    pub price_min: i64,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub concurrency_free: i32,
    /// Whether the publisher is willing to serve this lane *right now*.
    ///
    /// A publisher that has paused a model locally (cooling off after upstream
    /// errors, quota spent, waiting for its operator) keeps declaring the lane
    /// with `available: false` rather than dropping it from the snapshot: the
    /// mirror stays complete, so the seller console can say *why* nothing is
    /// selling, while matching still treats the lane as gone. Defaults to true
    /// so older clients keep the previous meaning.
    #[serde(default = "default_true")]
    pub available: bool,
    /// Machine-readable pause cause when `available` is false: `rate_limit`,
    /// `quota`, `auth`, `breaker`, `manual`.
    #[serde(default)]
    pub paused_reason: String,
    /// Unix seconds after which the publisher expects to be able to serve
    /// again; 0 when it does not know, or when it needs its operator to act.
    #[serde(default)]
    pub resume_at: i64,
}

fn default_true() -> bool {
    true
}

impl SupplyItem {
    /// A lane that is on offer with no restriction — the common case.
    pub fn offered(
        model: &str,
        provider: Provider,
        window_remaining: i64,
        price_min: i64,
        region: &str,
        concurrency_free: i32,
    ) -> SupplyItem {
        SupplyItem {
            model: model.to_string(),
            provider,
            window_remaining,
            price_min,
            region: region.to_string(),
            concurrency_free,
            available: true,
            paused_reason: String::new(),
            resume_at: 0,
        }
    }

    /// The same lane, withheld: declared so it stays visible and explainable,
    /// but not matchable.
    pub fn paused(mut self, reason: &str, resume_at: i64) -> SupplyItem {
        self.available = false;
        self.paused_reason = reason.to_string();
        self.resume_at = resume_at;
        self
    }
}

/// A `control` frame instructs the publisher to change behaviour (§9.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPayload {
    /// One of `kick` | `throttle` | `upgrade` | `lane.pause`.
    pub action: String,
    /// Optional human-readable reason.
    #[serde(default)]
    pub reason: String,
    /// For `throttle`: how long to slow down, in milliseconds.
    #[serde(default)]
    pub throttle_ms: i64,
    /// For `lane.pause`: which model the gateway took out of rotation.
    #[serde(default)]
    pub model: String,
    /// For `lane.pause`: whether it stays out until the operator acts.
    #[serde(default)]
    pub resume_requires_user: bool,
}

/// Usage reported back on `stream_end` / `http_response`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
}

impl Usage {
    pub fn total(&self) -> i64 {
        self.input_tokens + self.output_tokens
    }

    pub fn by_type(&self, tt: TokenType) -> i64 {
        match tt {
            TokenType::Input => self.input_tokens,
            TokenType::Output => self.output_tokens,
            TokenType::CacheRead => self.cache_read_tokens,
            TokenType::CacheWrite => self.cache_write_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supply_item_wire_names_are_snake_case() {
        let item = SupplyItem {
            model: "claude-opus-5".into(),
            provider: Provider::Claude,
            window_remaining: 1000,
            price_min: 3000,
            region: String::new(),
            concurrency_free: 4,
            available: true,
            paused_reason: String::new(),
            resume_at: 0,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["provider"], "claude");
        assert_eq!(v["window_remaining"], 1000);
        assert_eq!(v["concurrency_free"], 4);
    }

    #[test]
    fn an_older_publisher_omitting_the_pause_fields_still_reads_as_available() {
        let item: SupplyItem = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-5",
            "provider": "claude",
            "window_remaining": 10,
            "price_min": 1,
        }))
        .unwrap();
        assert!(item.available);
        assert_eq!(item.paused_reason, "");
        assert_eq!(item.resume_at, 0);
    }
}
