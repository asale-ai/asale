//! Frame payload bodies (spec §2.4).

use crate::ids::{Provider, TokenType, Wire};
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
    /// The lowest ratio this lane will sell at, in whole percent *of* the
    /// vendor's list price — the same scale the market ratio is published on.
    ///
    /// This is the publisher's reserve price, and the market needs it for one
    /// reason: with buyers absent, the only honest reading of the price is the
    /// best ask, and without this the gateway had no idea what any ask was. It
    /// priced an idle minute at `ratio_min` instead, which walked the price
    /// under every seller's floor, took them off the market, and — with the
    /// market now empty — walked it back up until they returned. That loop is
    /// self-sustaining and needs no buyer to keep running.
    ///
    /// `0` means "not declared": an older publisher, or a lane whose account
    /// has no floor. The gateway falls back to the configured `ratio_min`.
    #[serde(default)]
    pub ask_ratio: i32,
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
    /// The wire format this lane's upstream speaks, when its provider does not
    /// settle that on its own.
    ///
    /// Only a `custom` lane fills this in: its host is the operator's, so the
    /// dialect is a property of the account rather than of the provider. Every
    /// other provider's upstream is the vendor's own and its wire is known to
    /// the gateway at compile time.
    ///
    /// Empty means "whatever the provider implies" — which is also what an
    /// older publisher sends, and what a `custom` lane meant back when every
    /// such endpoint was assumed to speak the OpenAI schema.
    #[serde(default)]
    pub wire: String,

    /// An opaque, stable mark for the credential currently behind this lane.
    ///
    /// A lane's identity is `(device, provider, model)`, and none of those move
    /// when the seller signs the account in again, swaps in a different
    /// subscription under the same address, or repoints a custom endpoint. The
    /// thing that was checked has changed while everything naming it has
    /// stayed put — so a verdict keyed on the lane alone goes on vouching for
    /// something nobody looked at.
    ///
    /// This is what closes that. It is a hash, never the credential: the
    /// gateway compares it with the one attached to the last verdict and asks
    /// for a fresh verification when they differ. It cannot be reversed into a
    /// token, and it is not a stable identifier across devices — see
    /// `credential_fp` on the client for why it is salted per install.
    ///
    /// Empty from an older publisher, which reads as "no opinion": the verdict
    /// stands on its expiry alone, exactly as before this field existed.
    #[serde(default)]
    pub credential_fp: String,
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
            ask_ratio: 0,
            region: region.to_string(),
            concurrency_free,
            available: true,
            paused_reason: String::new(),
            resume_at: 0,
            wire: String::new(),
            credential_fp: String::new(),
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

    /// The same lane, carrying the floor its account will not sell below.
    pub fn asking(mut self, ask_ratio: i32) -> SupplyItem {
        self.ask_ratio = ask_ratio;
        self
    }

    /// The same lane, speaking a dialect its provider does not imply. Only a
    /// `custom` lane has one to declare.
    pub fn speaking(mut self, wire: Wire) -> SupplyItem {
        self.wire = wire.as_str().to_string();
        self
    }

    /// The declared wire, or `None` for a lane that left it to its provider —
    /// an older publisher, or any provider whose upstream is the vendor's own.
    /// An unrecognised value reads as `None` too: a gateway that does not know
    /// the dialect cannot build for it, and falling back to the provider's is
    /// the same answer it would have given before the field existed.
    pub fn declared_wire(&self) -> Option<Wire> {
        Wire::from_str_opt(&self.wire)
    }
}

/// A `control` frame instructs the publisher to change behaviour (§9.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPayload {
    /// One of `kick` | `throttle` | `upgrade` | `lane.pause` | `seller.status` |
    /// `task.cancel`.
    pub action: String,
    /// Optional human-readable reason.
    #[serde(default)]
    pub reason: String,
    /// For `task.cancel`: the relayed call to stop working on, because its
    /// consumer went away. Everything generated past this point is the seller's
    /// own subscription quota spent on an answer nobody will read — and unpaid,
    /// since the gateway bills only what reached it before the buyer left.
    #[serde(default)]
    pub task_id: String,
    /// For `throttle`: how long to slow down, in milliseconds.
    #[serde(default)]
    pub throttle_ms: i64,
    /// For `lane.pause`: which model the gateway took out of rotation.
    #[serde(default)]
    pub model: String,
    /// For `lane.pause`: the lane the gateway means, `{device}|{provider}`.
    /// The trailing segment is what lets a client selling one model through
    /// two providers pause only the one the gateway named (C3). Empty from an
    /// older gateway, which pauses every provider's lane for the model.
    #[serde(default)]
    pub lane: String,
    /// For `lane.pause`: whether it stays out until the operator acts.
    #[serde(default)]
    pub resume_requires_user: bool,
    /// For `seller.status`: this seller's reputation, and the floor matching
    /// applies to it. `score < min_score` means every lane this device declares
    /// is served only after the healthier ones — on any model that has other
    /// supply, close to never.
    ///
    /// Sent because a seller at the back of the queue has *no other symptom*:
    /// the client is connected, its lanes are declared, its console says it is
    /// selling, and no request arrives. On 2026-08-05 that state lasted hours
    /// and was only diagnosable by reading the gateway's Redis by hand.
    #[serde(default)]
    pub score: i32,
    #[serde(default)]
    pub min_score: i32,
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

    /// Everything the upstream account really spent on this call.
    ///
    /// Distinct from [`total`](Self::total), which answers "was anything
    /// served" and deliberately counts only the two sides a consumer receives.
    /// Quota decay is a different question: a cached prompt token still came
    /// out of the publisher's subscription window, and `input_tokens` holds
    /// only the *uncached* remainder once the cache fields are populated. Left
    /// as `input + output`, an OpenAI-dialect lane serving a 33k prompt with 30k
    /// of it cached would report ~3k against its window instead of ~33k, and
    /// oversell itself by an order of magnitude.
    pub fn quota_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
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
            ask_ratio: 60,
            region: String::new(),
            concurrency_free: 4,
            available: true,
            paused_reason: String::new(),
            resume_at: 0,
            wire: String::new(),
            credential_fp: "a1b2c3d4".into(),
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["provider"], "claude");
        assert_eq!(v["window_remaining"], 1000);
        assert_eq!(v["concurrency_free"], 4);
        assert_eq!(v["ask_ratio"], 60);
        assert_eq!(v["credential_fp"], "a1b2c3d4");
    }

    /// A gateway that has not been updated must still parse a frame carrying
    /// the field, and a publisher that has not been updated must still produce
    /// one this side accepts. Both halves of the rollout run at once.
    #[test]
    fn a_declaration_without_a_credential_mark_still_parses() {
        let older = serde_json::json!({
            "model": "claude-opus-5",
            "provider": "claude",
            "window_remaining": 1000,
            "price_min": 3000,
        });
        let item: SupplyItem = serde_json::from_value(older).unwrap();
        assert_eq!(item.credential_fp, "", "absent reads as not stated, never as a mismatch");
        assert!(item.available, "and the other defaults are unchanged");
    }

    /// A publisher built before the field existed declares no floor, and the
    /// gateway must read that as "no opinion" rather than "sells at zero" —
    /// which would hand the pricing loop an ask under every real one.
    #[test]
    fn an_older_publisher_declaring_no_floor_reads_as_zero() {
        let item: SupplyItem = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-5",
            "provider": "claude",
            "window_remaining": 1000,
            "price_min": 3000,
        }))
        .unwrap();
        assert_eq!(item.ask_ratio, 0);
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
        // The wire came later still: no field means the lane's provider settles
        // its own dialect, which is what every lane meant before this existed.
        assert_eq!(item.declared_wire(), None);
    }

    #[test]
    fn a_custom_lane_carries_the_dialect_its_endpoint_speaks() {
        let item = SupplyItem::offered("claude-opus-5", Provider::Custom, 10, 1, "", 2)
            .speaking(Wire::Claude);
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["wire"], "claude");
        let back: SupplyItem = serde_json::from_value(v).unwrap();
        assert_eq!(back.declared_wire(), Some(Wire::Claude));
    }

    #[test]
    fn a_dialect_this_build_does_not_know_reads_as_undeclared() {
        // A newer publisher offering a wire this gateway cannot build for. It
        // must not parse as *some* dialect: falling back to the provider's is
        // the answer this build would have given anyway, and guessing would put
        // a body the endpoint cannot read on the wire.
        let item: SupplyItem = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-5",
            "provider": "custom",
            "window_remaining": 10,
            "price_min": 1,
            "wire": "bedrock",
        }))
        .unwrap();
        assert_eq!(item.declared_wire(), None);
    }
}
