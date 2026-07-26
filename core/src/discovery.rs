//! Subscription discovery (spec §3). The `ToolAdapter` trait abstracts each
//! provider's OAuth refresh + quota. Concrete adapters (claude/gemini/…)
//! implement it; the Tauri layer drives the OAuth browser flow and keychain.
//!
//! Quota probing (§P0-1): the upstreams expose no public "remaining tokens" API
//! for OAuth subscriptions, so we estimate the rolling window from the account's
//! plan cap minus the tokens this device has served in the last window (tracked
//! locally in `provider_records`). This is a real local-measurement estimate,
//! not a hard-coded constant — the caller supplies `used_in_window`.

use async_trait::async_trait;
use std::time::Duration;

/// Providers the client can publish (claude_work shares the claude upstream).
///
/// Defined in `asale-protocol` because the provider name travels on the wire in
/// every `supply.declare`; the server matches on the same enum.
pub use asale_protocol::ids::Provider;

/// A held subscription token (access token kept in keychain; here we carry the
/// reference + expiry metadata).
#[derive(Debug, Clone)]
pub struct AccountToken {
    pub provider: Provider,
    pub account_id: String,
    pub keychain_ref: String,
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
}

/// The result of a successful token refresh (spec §3.4).
#[derive(Debug, Clone)]
pub struct RefreshedToken {
    pub access_token: String,
    /// Some providers (Google) do not rotate the refresh token; then this is None.
    pub refresh_token: Option<String>,
    /// Absolute unix seconds when the new access token expires, if known.
    pub expires_at: Option<i64>,
}

/// A rate-window quota snapshot (spec §3.1 — not token balance, §P0-1).
#[derive(Debug, Clone, Copy)]
pub enum WindowKind {
    Rolling5h,
    Weekly,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaState {
    Available,
    Limited,
    Exhausted,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct QuotaWindow {
    pub kind: WindowKind,
    pub state: QuotaState,
    /// Estimated serviceable tokens remaining in the window.
    pub est_serviceable_tokens: u64,
    /// 0..1 fraction remaining, when derivable.
    pub remaining_ratio: Option<f32>,
    pub reset_at: Option<i64>,
    /// Provenance of the estimate (e.g. "plan-cap:pro − local-5h-usage").
    pub source: String,
}

/// The upstream endpoint info a publisher injects into (base URL + default UA).
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    pub base_url: String,
    pub default_headers: Vec<(String, String)>,
}

// ── Plan → rolling-window cap (best-effort, documented provenance) ──────────
//
// Upstreams don't publish exact token caps for OAuth plans; these are
// conservative published-tier estimates for the 5h rolling window used only as
// the *ceiling* for the local-usage subtraction below. When the plan is
// unknown we assume the lowest paid tier so we never over-declare supply.

/// Estimated serviceable tokens for a provider+plan over its rate window.
pub fn plan_window_cap(provider: Provider, plan: Option<&str>) -> u64 {
    let p = plan.unwrap_or("").to_ascii_lowercase();
    match provider {
        Provider::Claude | Provider::ClaudeWork => {
            if p.contains("max20") || p.contains("max_20") || p.contains("max 20") {
                4_400_000
            } else if p.contains("max") {
                2_200_000
            } else if p.contains("team") || p.contains("enterprise") || p.contains("work") {
                1_500_000
            } else if p.contains("pro") {
                1_100_000
            } else {
                // Free / unknown: lowest paid-tier estimate, stay conservative.
                220_000
            }
        }
        Provider::Codex => {
            if p.contains("pro") {
                2_000_000
            } else if p.contains("plus") {
                1_000_000
            } else {
                200_000
            }
        }
        Provider::Gemini => {
            if p.contains("ultra") {
                3_000_000
            } else if p.contains("pro") || p.contains("advanced") {
                1_500_000
            } else {
                300_000
            }
        }
        Provider::Kimi | Provider::Xai => 500_000,
    }
}

/// Build a rolling-window quota estimate from the plan cap and locally measured
/// usage in the window. This is the real §P0-1 estimate: cap − used.
pub fn estimate_quota_window(
    provider: Provider,
    plan: Option<&str>,
    used_in_window: u64,
    reset_at: Option<i64>,
) -> QuotaWindow {
    let cap = plan_window_cap(provider, plan);
    let remaining = cap.saturating_sub(used_in_window);
    let ratio = if cap > 0 { (remaining as f32 / cap as f32).clamp(0.0, 1.0) } else { 0.0 };
    let state = if remaining == 0 {
        QuotaState::Exhausted
    } else if ratio < 0.15 {
        QuotaState::Limited
    } else {
        QuotaState::Available
    };
    let kind = match provider {
        Provider::Claude | Provider::ClaudeWork => WindowKind::Rolling5h,
        _ => WindowKind::Rolling5h,
    };
    QuotaWindow {
        kind,
        state,
        est_serviceable_tokens: remaining,
        remaining_ratio: Some(ratio),
        reset_at,
        source: format!(
            "plan-cap({}={}) − local-window-usage({})",
            plan.unwrap_or("unknown"),
            cap,
            used_in_window
        ),
    }
}

/// Per-provider adapter contract (spec §3.1).
#[async_trait]
pub trait ToolAdapter: Send + Sync {
    fn provider(&self) -> Provider;

    /// Refresh an access token using the stored refresh token (spec §3.4).
    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<RefreshedToken>;

    /// How long before expiry to proactively refresh (spec §3.1 `refresh_lead`).
    fn refresh_lead(&self) -> Duration {
        Duration::from_secs(300)
    }

    /// Query the rolling-window quota for supply estimation. `used_in_window` is
    /// the tokens this device already served this window (from local records).
    async fn query_quota(&self, token: &AccountToken, used_in_window: u64) -> anyhow::Result<QuotaWindow> {
        Ok(estimate_quota_window(self.provider(), token.plan.as_deref(), used_in_window, None))
    }

    /// The upstream endpoint + UA profile for this provider.
    fn upstream(&self) -> UpstreamSpec;
}

/// Turn a token-endpoint response into a `RefreshedToken`, reporting the HTTP
/// status when the endpoint refused us.
///
/// The status matters more than the body here: a 403 from a provider's token
/// endpoint is not an OAuth error at all, it is the edge refusing the
/// connection (region blocks answer even unauthenticated endpoints this way),
/// and the fix is a proxy — not a re-login. Without the status the caller only
/// saw "missing access_token" and could not tell the two apart.
pub async fn parse_token_response(provider: &str, resp: reqwest::Response) -> anyhow::Result<RefreshedToken> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        return parse_refresh_response(&v);
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!(
            "{provider} token endpoint refused the request (HTTP 403: {body}). \
             This is the provider blocking the connection itself, not an expired login — \
             set {} to a reachable proxy (e.g. http://127.0.0.1:7890) and restart the daemon.",
            crate::http::PROXY_ENV
        );
    }
    anyhow::bail!("{provider} token refresh failed (HTTP {status}): {body}")
}

/// Parse a token endpoint JSON response into a `RefreshedToken`.
pub fn parse_refresh_response(v: &serde_json::Value) -> anyhow::Result<RefreshedToken> {
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("refresh response missing access_token: {v}"))?
        .to_string();
    let refresh = v.get("refresh_token").and_then(|x| x.as_str()).map(String::from);
    let expires_at = v.get("expires_in").and_then(|x| x.as_i64()).map(|secs| now_secs() + secs);
    Ok(RefreshedToken { access_token: access, refresh_token: refresh, expires_at })
}

/// Whether a token with `expires_at` should be refreshed now given the lead.
pub fn needs_refresh(expires_at: Option<i64>, lead: Duration, now: i64) -> bool {
    match expires_at {
        Some(exp) => now + lead.as_secs() as i64 >= exp,
        None => false, // unknown expiry: refresh reactively on 401, not here.
    }
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ── Claude / Claude Work adapter ────────────────────────────────────────────

/// Claude adapter (also the base for ClaudeWork, which only differs in the UA
/// profile; both share the same Anthropic OAuth client_id — confirmed against
/// CLIProxyAPI `internal/auth/claude/anthropic_auth.go`).
pub struct ClaudeAdapter {
    pub work: bool,
    pub client_id: String,
}

impl ClaudeAdapter {
    pub fn new(work: bool, client_id: impl Into<String>) -> ClaudeAdapter {
        ClaudeAdapter { work, client_id: client_id.into() }
    }
}

#[async_trait]
impl ToolAdapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        if self.work {
            Provider::ClaudeWork
        } else {
            Provider::Claude
        }
    }

    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<RefreshedToken> {
        // Anthropic OAuth token endpoint accepts a JSON body (§3.2).
        let resp = crate::http::upstream()
            .post("https://api.anthropic.com/v1/oauth/token")
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": self.client_id,
            }))
            .send()
            .await?;
        parse_token_response("claude", resp).await
    }

    fn upstream(&self) -> UpstreamSpec {
        // Distinct UA profiles so the same account family's two supply kinds are
        // distinguishable upstream (§3.2).
        let ua = if self.work {
            "claude-work/1.0 (desktop)"
        } else {
            "claude-cli/1.0 (external, cli)"
        };
        UpstreamSpec {
            base_url: "https://api.anthropic.com/v1/messages".into(),
            default_headers: vec![
                ("anthropic-version".into(), "2023-06-01".into()),
                ("user-agent".into(), ua.into()),
            ],
        }
    }
}

// ── Codex adapter ───────────────────────────────────────────────────────────

/// Codex (ChatGPT OAuth) adapter. The client_id is the public identifier from
/// the official Codex CLI (see cc-switch codex_oauth_auth.rs / CLIProxyAPI
/// openai_auth.go). Refresh is a form-encoded call to auth.openai.com.
pub struct CodexAdapter {
    pub client_id: String,
}

impl CodexAdapter {
    pub fn new(client_id: impl Into<String>) -> CodexAdapter {
        CodexAdapter { client_id: client_id.into() }
    }
}

#[async_trait]
impl ToolAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<RefreshedToken> {
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.client_id.as_str()),
            ("scope", "openid profile email"),
        ];
        let resp = crate::http::upstream().post("https://auth.openai.com/oauth/token").form(&params).send().await?;
        let mut t = parse_token_response("codex", resp).await?;
        // OpenAI may omit the refresh token on refresh: keep the existing one.
        if t.refresh_token.is_none() {
            t.refresh_token = Some(refresh_token.to_string());
        }
        Ok(t)
    }

    fn upstream(&self) -> UpstreamSpec {
        UpstreamSpec {
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            default_headers: vec![("user-agent".into(), "codex-cli".into())],
        }
    }
}

// ── Gemini adapter ──────────────────────────────────────────────────────────

/// Gemini (gemini-cli installed-app) adapter. client_id/secret are the public
/// values published in gemini-cli source (see cc-switch `subscription.rs`).
pub struct GeminiAdapter {
    pub client_id: String,
    pub client_secret: String,
}

impl GeminiAdapter {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> GeminiAdapter {
        GeminiAdapter { client_id: client_id.into(), client_secret: client_secret.into() }
    }
}

#[async_trait]
impl ToolAdapter for GeminiAdapter {
    fn provider(&self) -> Provider {
        Provider::Gemini
    }

    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<RefreshedToken> {
        // Google token endpoint uses form-encoded params and does not rotate the
        // refresh token.
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ];
        let resp = crate::http::upstream().post("https://oauth2.googleapis.com/token").form(&params).send().await?;
        let mut t = parse_token_response("gemini", resp).await?;
        // Google omits refresh_token on refresh: keep the existing one.
        if t.refresh_token.is_none() {
            t.refresh_token = Some(refresh_token.to_string());
        }
        Ok(t)
    }

    fn upstream(&self) -> UpstreamSpec {
        UpstreamSpec {
            base_url: "https://generativelanguage.googleapis.com".into(),
            default_headers: vec![("user-agent".into(), "gemini-cli/1.0".into())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_estimate_subtracts_local_usage() {
        // Pro plan cap minus what we already served this window.
        let q = estimate_quota_window(Provider::Claude, Some("pro"), 100_000, None);
        let cap = plan_window_cap(Provider::Claude, Some("pro"));
        assert_eq!(q.est_serviceable_tokens, cap - 100_000);
        assert_eq!(q.state, QuotaState::Available);
        assert!(q.remaining_ratio.unwrap() > 0.8);
        assert!(q.source.contains("plan-cap"));
    }

    #[test]
    fn quota_exhausts_and_limits() {
        let cap = plan_window_cap(Provider::Claude, Some("pro"));
        let full = estimate_quota_window(Provider::Claude, Some("pro"), cap, None);
        assert_eq!(full.state, QuotaState::Exhausted);
        assert_eq!(full.est_serviceable_tokens, 0);

        let near = estimate_quota_window(Provider::Claude, Some("pro"), cap - (cap / 20), None);
        assert_eq!(near.state, QuotaState::Limited);
    }

    #[test]
    fn unknown_plan_is_conservative() {
        // Unknown plan must not exceed the lowest tier estimate.
        let unknown = plan_window_cap(Provider::Claude, None);
        let pro = plan_window_cap(Provider::Claude, Some("pro"));
        assert!(unknown < pro);
    }

    #[test]
    fn needs_refresh_respects_lead() {
        let now = 1_000_000i64;
        let lead = Duration::from_secs(300);
        // Expires in 200s (< lead) → refresh now.
        assert!(needs_refresh(Some(now + 200), lead, now));
        // Expires in 1h → not yet.
        assert!(!needs_refresh(Some(now + 3600), lead, now));
        // Unknown expiry → don't proactively refresh.
        assert!(!needs_refresh(None, lead, now));
    }

    #[test]
    fn parse_refresh_ok_and_err() {
        let v = serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":3600});
        let t = parse_refresh_response(&v).unwrap();
        assert_eq!(t.access_token, "a");
        assert_eq!(t.refresh_token.as_deref(), Some("r"));
        assert!(t.expires_at.is_some());

        let bad = serde_json::json!({"error":"invalid_grant"});
        assert!(parse_refresh_response(&bad).is_err());
    }
}
