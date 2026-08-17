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
pub use asale_protocol::ids::{Provider, Wire};

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

/// Testing override for the window cap below, in tokens. Unset (or 0) keeps the
/// plan estimate.
///
/// The caps here are guesses, and a deliberately low-balled one: an account that
/// has served its estimated 5h allowance declares `quota_remaining = 0` and its
/// lanes leave the market, whatever the real upstream would still accept. On a
/// development machine that is the normal state within an afternoon of testing,
/// and the only way back is to wait out the rolling window — which makes the buy
/// path untestable for hours at a time, for a number nobody measured.
///
/// Not a production knob: over-declaring supply means matching sends work to a
/// lane the upstream then refuses, which costs the publisher reputation.
const WINDOW_CAP_OVERRIDE_ENV: &str = "ASALE_PLAN_WINDOW_CAP";

/// Estimated serviceable tokens for a provider+plan over its rate window.
pub fn plan_window_cap(provider: Provider, plan: Option<&str>) -> u64 {
    if let Some(cap) = std::env::var(WINDOW_CAP_OVERRIDE_ENV).ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|c| *c > 0) {
        return cap;
    }
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
        // Kimi Code and the Grok CLI subscription publish no token allowance —
        // their limits are expressed as requests over a window, not tokens —
        // and the platform APIs are metered against a balance rather than
        // capped at all. Neither can be asked about, so this is a deliberately
        // conservative window; the per-account daily cap on the Sell page is
        // the control that actually matters for these four.
        Provider::Kimi | Provider::KimiApi | Provider::Xai | Provider::XaiApi => 500_000,
        // A custom endpoint has no rolling window to estimate: it is a metered
        // key against somebody's balance, and the estimate exists to keep a
        // *subscription* from over-declaring what its plan allows. A realistic
        // number here would take the account off the market after an afternoon
        // for a limit that does not exist, so the window is effectively open and
        // the per-account daily cap is the control that bounds it.
        Provider::Custom => CUSTOM_WINDOW_TOKENS,
    }
}

/// The window declared for a custom endpoint. Large enough to never be the
/// binding constraint, finite so the lane still declares a number the market can
/// reason about rather than an unbounded one.
pub const CUSTOM_WINDOW_TOKENS: u64 = 100_000_000;

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

// ── Which plan a Claude subscription is on ──────────────────────────────────
//
// Anthropic's OAuth token exchange does not say. The response carries
// `account` (email + uuid) and `organization`, and neither has ever held a
// `subscription_type` — which is what `account_plan` looks for, and why every
// Claude login lands on the unknown-plan branch of `plan_window_cap` and sells
// like the lowest paid tier. A Max 20× subscription was being metered at 220k
// tokens per five hours instead of 4.4M: a factor of twenty.
//
// `oauth/profile` does say, three ways, and this reads all three because they
// disagree in precision:
//
// ```json
// { "account":      { "has_claude_max": true, "has_claude_pro": false },
//   "organization": { "organization_type": "claude_max",
//                     "rate_limit_tier":   "default_claude_max_20x" } }
// ```
//
// `rate_limit_tier` is the only one that separates Max 5× from Max 20×, and it
// is named after the thing we are trying to size — the rate limit itself.

/// A Claude plan label from an `oauth/profile` body (or any other object
/// carrying the same `account`/`organization` pair — the token exchange's
/// response has the same shape, it just usually says less).
///
/// The strings returned are the vocabulary [`plan_window_cap`] matches on, not
/// Anthropic's: `max20`, `max5`, `max`, `team`, `pro`. An unrecognised tier
/// falls through to the coarser fields rather than being passed along verbatim,
/// so a tier name we have never seen cannot silently match `contains("pro")`
/// and mis-size a Max account.
///
/// Precision is deliberately lost downward, never upward: `organization_type`
/// says `claude_max` without saying which multiple, and that resolves to `max`
/// (2.2M, the 5× estimate) rather than to the 20× one. Over-declaring supply
/// sends the market work the upstream then refuses, which costs this device its
/// reputation; under-declaring only costs an offer.
pub fn claude_plan_from_profile(body: &serde_json::Value) -> Option<String> {
    let org = &body["organization"];
    let tier = org["rate_limit_tier"].as_str().unwrap_or("").to_ascii_lowercase();
    // `default_claude_max_20x`, `default_claude_max_5x`, `default_claude_pro`…
    let from_tier = if tier.contains("max") && (tier.contains("20x") || tier.contains("_20")) {
        Some("max20")
    } else if tier.contains("max") {
        Some("max5")
    } else if tier.contains("team") || tier.contains("enterprise") {
        Some("team")
    } else if tier.contains("pro") {
        Some("pro")
    } else {
        None
    };
    if let Some(p) = from_tier {
        return Some(p.to_string());
    }
    // `claude_max` / `claude_pro` / `claude_team`: the plan without the multiple.
    let kind = org["organization_type"].as_str().unwrap_or("").to_ascii_lowercase();
    let from_kind = if kind.contains("max") {
        Some("max")
    } else if kind.contains("team") || kind.contains("enterprise") {
        Some("team")
    } else if kind.contains("pro") {
        Some("pro")
    } else {
        None
    };
    if let Some(p) = from_kind {
        return Some(p.to_string());
    }
    // The booleans on the account itself — the last thing left, and still worth
    // twice the unknown-plan default.
    let acct = &body["account"];
    if acct["has_claude_max"].as_bool() == Some(true) {
        return Some("max".into());
    }
    if acct["has_claude_pro"].as_bool() == Some(true) {
        return Some("pro".into());
    }
    None
}

/// Read `api.anthropic.com/api/oauth/profile` with a subscription's own bearer.
///
/// Free — it spends no subscription quota — and the only place the plan is
/// stated at all. Region blocks answer it the same way they answer everything
/// else (403 "Request not allowed"), so the caller treats a failure as "no
/// answer yet" and keeps whatever plan it already had.
pub async fn fetch_claude_profile(access_token: &str) -> anyhow::Result<serde_json::Value> {
    let resp = crate::http::upstream()
        .get("https://api.anthropic.com/api/oauth/profile")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(12))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("oauth/profile refused the request (HTTP {status}): {}", body.chars().take(200).collect::<String>());
    }
    Ok(resp.json().await?)
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

/// The Codex CLI version the model list is asked for.
///
/// `/backend-api/codex/models` answers per calling version: every entry carries
/// a `minimal_client_version`, and a request naming an older one is answered
/// with an empty list rather than an error (`client_version=0.50.0` returns
/// `{"models":[]}` while `0.146.0` returns six models). Keep this in step with
/// the user-agent the relay sends upstream — `translator::responses::CODEX_UA`
/// on the server — or this device would advertise a model the relay then
/// addresses as a client too old to be given it.
pub const CODEX_CLIENT_VERSION: &str = "0.146.0";

/// The model slugs a ChatGPT account's Codex surface will actually serve.
///
/// A Codex subscription is not entitled to OpenAI's platform model list. The
/// backend serves a per-account, per-plan set that moves with each release and
/// refuses every other slug — including plain `gpt-5.1`, `gpt-5-codex` and
/// `gpt-5.1-codex` — with the same answer regardless of the rest of the body:
///
/// ```text
/// 400 {"detail":"The 'gpt-5.1' model is not supported when using Codex with a ChatGPT account."}
/// ```
///
/// So a lane advertising a model from the catalog alone fails at the upstream
/// call, *after* the request was matched, preauthorized and routed — the
/// publisher wears a failure that was never its fault. The entitled set is only
/// knowable by asking, which is what this does.
///
/// Every returned slug counts, `visibility` included: that field decides what
/// the ChatGPT app's own picker offers, not what the account may ask for.
/// An empty list is a real answer — "this account may not use this surface" —
/// and not an error.
pub async fn codex_servable_models(token: &str, chatgpt_account_id: &str) -> anyhow::Result<Vec<String>> {
    let mut req = crate::http::upstream()
        .get(format!(
            "https://chatgpt.com/backend-api/codex/models?client_version={CODEX_CLIENT_VERSION}"
        ))
        .header("authorization", format!("Bearer {token}"))
        .header("originator", "codex_cli_rs")
        .header("user-agent", format!("codex_cli_rs/{CODEX_CLIENT_VERSION}"))
        .timeout(Duration::from_secs(20));
    // Known for an account asale logged in itself; absent on older rows, where
    // the bearer's own claim is the fallback (same as the executor's).
    if !chatgpt_account_id.is_empty() {
        req = req.header("chatgpt-account-id", chatgpt_account_id);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!("codex models {status}: {body}");
    }
    Ok(body
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("slug").and_then(|s| s.as_str()))
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default())
}

// ── Custom endpoint ─────────────────────────────────────────────────────────

/// The models a custom endpoint says it serves.
///
/// `GET {base}/models` is the one call all four dialects answer — Anthropic and
/// Google publish it under the same path OpenAI does — so this doubles as the
/// credential check when an account is connected: a base URL that is not an
/// endpoint of the declared dialect, or a key it refuses, fails here rather
/// than on the first consumer request.
///
/// Ids are returned exactly as the endpoint spells them. Deciding which of them
/// the market can actually trade is the caller's job: that answer belongs to the
/// platform's catalog, not to the endpoint.
pub async fn custom_endpoint_models(base_url: &str, api_key: &str, wire: Wire) -> anyhow::Result<Vec<String>> {
    let base = base_url.trim().trim_end_matches('/');
    anyhow::ensure!(
        base.starts_with("http://") || base.starts_with("https://"),
        "base URL must start with http:// or https://"
    );
    let url = format!("{base}/models");
    let req = crate::http::upstream().get(&url).timeout(Duration::from_secs(30));
    // The same key, in the header each dialect's hosts read it from — the probe
    // has to fail for the *right* reason, and a bearer sent to an Anthropic
    // endpoint 401s exactly like a bad key.
    let req = match wire {
        Wire::Openai | Wire::Responses => req.header("authorization", format!("Bearer {api_key}")),
        Wire::Claude => req.header("x-api-key", api_key).header("anthropic-version", "2023-06-01"),
        Wire::Gemini => req.header("x-goog-api-key", api_key),
    };
    let resp = req.send().await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}: {body}");
    }
    // Three shapes for one answer: `{"data":[{"id":…}]}` is OpenAI's and
    // Anthropic's, `{"models":[{"name":"models/…"}]}` is Google's, and a bare
    // array is what a few proxies reply with.
    let items = body
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| body.get("models").and_then(|m| m.as_array()))
        .or_else(|| body.as_array())
        .ok_or_else(|| anyhow::anyhow!("{url} did not return a model list"))?;
    let mut models: Vec<String> = items
        .iter()
        .filter_map(|m| {
            m.get("id")
                .and_then(|i| i.as_str())
                // Google names them `models/gemini-3.5-flash`; the id the rest
                // of the world uses is the tail, and it is what the catalog and
                // the request path both want.
                .or_else(|| m.get("name").and_then(|n| n.as_str()).map(|n| n.trim_start_matches("models/")))
                .or_else(|| m.as_str())
        })
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    models.sort();
    models.dedup();
    Ok(models)
}

/// The dialect an endpoint answers on, found by asking.
///
/// Connecting one is the moment its operator is least sure what it speaks — a
/// reseller's docs say "OpenAI-compatible" for a host that also serves
/// `/messages`, and the answer decides every request afterwards. So the probe
/// tries each dialect in turn and reports the first that answers, rather than
/// making the operator guess and discover the mistake on the first sale.
///
/// Order is by how much supply each dialect actually carries. The error
/// returned is the first one's: it is the likeliest endpoint, so its refusal is
/// the most useful thing to show.
pub async fn detect_custom_endpoint_wire(
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<(Wire, Vec<String>)> {
    let mut first_err = None;
    for wire in [Wire::Openai, Wire::Claude, Wire::Gemini, Wire::Responses] {
        match custom_endpoint_models(base_url, api_key, wire).await {
            Ok(models) if !models.is_empty() => return Ok((wire, models)),
            // An endpoint that answers with an empty list is reachable and
            // authenticated but has nothing to sell. That is a real answer, not
            // a reason to try the next dialect — keep it only if nothing better
            // turns up.
            Ok(_) => {
                first_err.get_or_insert_with(|| {
                    anyhow::anyhow!("the endpoint reports no models, so there is nothing to sell")
                });
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.unwrap_or_else(|| anyhow::anyhow!("no dialect this client speaks answered")))
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

// ── Device-flow adapters (Kimi Code / Grok CLI subscriptions) ───────────────

/// Adapter for a coding subscription authorised by RFC 8628 device code.
///
/// Moonshot and xAI both ship their subscription through a CLI that authorises
/// this way: no redirect URI to register, the user approves a short code in a
/// browser, and the client polls for the token. Refresh is an ordinary
/// form-encoded `refresh_token` grant, which is all this adapter has to do —
/// the interactive half lives in [`crate::device_flow`].
///
/// Endpoints and client ids are the public values the vendor CLIs ship,
/// confirmed against CLIProxyAPI (`internal/auth/kimi`, `internal/auth/xai`).
pub struct DeviceFlowAdapter {
    provider: Provider,
    token_url: String,
    client_id: &'static str,
    base_url: &'static str,
    user_agent: &'static str,
}

impl DeviceFlowAdapter {
    pub fn kimi() -> DeviceFlowAdapter {
        DeviceFlowAdapter {
            provider: Provider::Kimi,
            token_url: crate::device_flow::KIMI_TOKEN_URL.to_string(),
            client_id: crate::device_flow::KIMI_CLIENT_ID,
            base_url: "https://api.kimi.com/coding",
            user_agent: "kimi-cli/1.0",
        }
    }

    /// xAI resolves its token endpoint through OIDC discovery. The discovered
    /// value is passed in so a refresh never has to make two calls; the
    /// published default is used when the caller has not discovered one yet.
    pub fn xai(token_url: Option<String>) -> DeviceFlowAdapter {
        DeviceFlowAdapter {
            provider: Provider::Xai,
            token_url: token_url.unwrap_or_else(|| crate::device_flow::XAI_TOKEN_URL_FALLBACK.to_string()),
            client_id: crate::device_flow::XAI_CLIENT_ID,
            base_url: "https://cli-chat-proxy.grok.com/v1",
            user_agent: "xai-grok-workspace/0.2.93",
        }
    }

    /// The adapter for a device-flow provider, or `None` for any other.
    pub fn for_provider(p: Provider) -> Option<DeviceFlowAdapter> {
        match p {
            Provider::Kimi => Some(DeviceFlowAdapter::kimi()),
            Provider::Xai => Some(DeviceFlowAdapter::xai(None)),
            _ => None,
        }
    }
}

#[async_trait]
impl ToolAdapter for DeviceFlowAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<RefreshedToken> {
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.client_id),
        ];
        let resp = crate::http::upstream()
            .post(&self.token_url)
            .header("accept", "application/json")
            .form(&params)
            .send()
            .await?;
        let mut t = parse_token_response(self.provider.as_str(), resp).await?;
        // Neither vendor is documented to rotate the refresh token, and both
        // omit it on refresh in practice — dropping it would log the publisher
        // out at the next renewal.
        if t.refresh_token.is_none() {
            t.refresh_token = Some(refresh_token.to_string());
        }
        Ok(t)
    }

    fn upstream(&self) -> UpstreamSpec {
        UpstreamSpec {
            base_url: self.base_url.into(),
            default_headers: vec![("user-agent".into(), self.user_agent.into())],
        }
    }
}

// ── API-key adapters (Moonshot platform / xAI platform) ─────────────────────

/// Adapter for a provider whose credential is a long-lived API key.
///
/// This is the *metered platform* half of each vendor, not the subscription:
/// there is no authorization code to exchange and no refresh token to rotate,
/// because the key the user pastes is the whole credential and stays valid
/// until they revoke it. That makes `refresh` unreachable in normal operation —
/// an API-key account is stored without an expiry, and `needs_refresh(None, ..)`
/// is false — so it fails loudly rather than pretending to have renewed
/// something.
///
/// One type covers both because the difference between them is a host and a
/// name, not behaviour.
pub struct ApiKeyAdapter {
    provider: Provider,
    base_url: &'static str,
    user_agent: &'static str,
}

impl ApiKeyAdapter {
    /// Moonshot's platform API. The base URL is the mainland deployment; a key
    /// issued by the global one (`api.moonshot.ai`) is rejected here and vice
    /// versa. Informational only — the gateway builds the URL actually called.
    pub fn kimi() -> ApiKeyAdapter {
        ApiKeyAdapter {
            provider: Provider::KimiApi,
            base_url: "https://api.moonshot.cn/v1",
            user_agent: "kimi-cli/1.0",
        }
    }

    pub fn xai() -> ApiKeyAdapter {
        ApiKeyAdapter {
            provider: Provider::XaiApi,
            base_url: "https://api.x.ai/v1",
            user_agent: "grok-cli/1.0",
        }
    }

    /// The adapter for an API-key provider, or `None` for one that uses OAuth.
    pub fn for_provider(p: Provider) -> Option<ApiKeyAdapter> {
        match p {
            Provider::KimiApi => Some(ApiKeyAdapter::kimi()),
            Provider::XaiApi => Some(ApiKeyAdapter::xai()),
            _ => None,
        }
    }
}

#[async_trait]
impl ToolAdapter for ApiKeyAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    async fn refresh(&self, _refresh_token: &str) -> anyhow::Result<RefreshedToken> {
        anyhow::bail!(
            "{} accounts authenticate with an API key, which never expires and cannot be refreshed — \
             if upstream is rejecting it the key was revoked or rotated, and the fix is to paste the new one",
            self.provider
        )
    }

    fn upstream(&self) -> UpstreamSpec {
        UpstreamSpec {
            base_url: self.base_url.into(),
            default_headers: vec![("user-agent".into(), self.user_agent.into())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_api_key_account_is_never_refreshed_and_says_why() {
        for p in [Provider::KimiApi, Provider::XaiApi] {
            let a = ApiKeyAdapter::for_provider(p).expect("api-key provider has an adapter");
            assert_eq!(a.provider(), p);
            // Stored with no expiry, so the refresh loop skips it outright —
            // this is what keeps `refresh` from ever being called in practice.
            assert!(!needs_refresh(None, a.refresh_lead(), 9_999_999_999));
            let e = a.refresh("whatever").await.unwrap_err().to_string();
            assert!(e.contains("API key"), "the error must name the real cause: {e}");
        }
        for p in [Provider::Claude, Provider::Gemini, Provider::Kimi, Provider::Xai] {
            assert!(ApiKeyAdapter::for_provider(p).is_none(), "{p} is not an API-key provider");
        }
    }

    #[test]
    fn each_flavour_of_a_vendor_points_at_its_own_host() {
        // The subscription and the platform API are different endpoints, and
        // the whole reason they are separate providers is that the credential
        // for one is refused by the other.
        assert_eq!(DeviceFlowAdapter::kimi().upstream().base_url, "https://api.kimi.com/coding");
        assert!(ApiKeyAdapter::kimi().upstream().base_url.contains("moonshot"));
        assert_eq!(DeviceFlowAdapter::xai(None).upstream().base_url, "https://cli-chat-proxy.grok.com/v1");
        assert_eq!(ApiKeyAdapter::xai().upstream().base_url, "https://api.x.ai/v1");
    }

    #[test]
    fn the_kimi_device_id_is_stable_and_uuid_shaped() {
        // Re-deriving it must give the same answer: a per-request id would make
        // one publisher look like many machines on one subscription.
        let a = crate::executor::kimi_device_id_for_test("me@example.com");
        assert_eq!(a, crate::executor::kimi_device_id_for_test("me@example.com"));
        assert_ne!(a, crate::executor::kimi_device_id_for_test("other@example.com"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    }

    #[test]
    fn device_flow_adapters_cover_exactly_the_subscription_providers() {
        for p in Provider::ALL {
            let expected = matches!(p, Provider::Kimi | Provider::Xai);
            assert_eq!(DeviceFlowAdapter::for_provider(p).is_some(), expected, "{p}");
        }
        // xAI discovers its token endpoint; the fallback stands in until it has.
        let discovered = DeviceFlowAdapter::xai(Some("https://auth.x.ai/oauth2/token-v2".into()));
        assert_eq!(discovered.provider(), Provider::Xai);
    }

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

    /// A real `oauth/profile` body, read off a production account on
    /// 2026-08-17. This is the account whose Max 20× subscription was being
    /// metered as an unknown plan — 220k tokens per five hours against a real
    /// allowance twenty times that — because the token exchange says none of
    /// this and nothing else was ever asked.
    fn real_profile() -> serde_json::Value {
        serde_json::json!({
            "account": {
                "uuid": "…", "full_name": "oh", "display_name": "oh",
                "email": "…", "has_claude_max": true, "has_claude_pro": false,
                "created_at": "2025-08-02T01:13:56.169093Z"
            },
            "organization": {
                "uuid": "…", "name": "…'s Organization",
                "organization_type": "claude_max",
                "billing_type": "stripe_subscription",
                "rate_limit_tier": "default_claude_max_20x",
                "seat_tier": null, "has_extra_usage_enabled": true,
                "subscription_status": "active"
            },
            "application": { "name": "Claude Code", "slug": "claude-code" }
        })
    }

    #[test]
    fn the_profile_sizes_the_subscription_the_token_exchange_would_not() {
        let plan = claude_plan_from_profile(&real_profile()).unwrap();
        assert_eq!(plan, "max20");
        assert_eq!(plan_window_cap(Provider::Claude, Some(&plan)), 4_400_000);
        // What it was doing before anyone asked.
        assert_eq!(plan_window_cap(Provider::Claude, None), 220_000);
    }

    #[test]
    fn every_tier_anthropic_publishes_lands_on_its_own_cap() {
        let with_tier = |tier: &str| {
            let body = serde_json::json!({ "organization": { "rate_limit_tier": tier } });
            claude_plan_from_profile(&body)
        };
        assert_eq!(with_tier("default_claude_max_20x").as_deref(), Some("max20"));
        assert_eq!(with_tier("default_claude_max_5x").as_deref(), Some("max5"));
        assert_eq!(with_tier("default_claude_pro").as_deref(), Some("pro"));
        assert_eq!(with_tier("default_claude_team").as_deref(), Some("team"));
        // Max 5× is the 2.2M estimate, not the 20× one.
        assert_eq!(plan_window_cap(Provider::Claude, Some("max5")), 2_200_000);
        assert_eq!(plan_window_cap(Provider::Claude, Some("team")), 1_500_000);
        assert_eq!(plan_window_cap(Provider::Claude, Some("pro")), 1_100_000);
    }

    /// The coarser fields are read only when the precise one says nothing, and
    /// they resolve *downward*: `claude_max` without a multiple is priced as
    /// the 5× tier. Declaring supply an upstream will refuse costs this device
    /// its reputation; declaring less than it has costs one offer.
    #[test]
    fn a_plan_without_its_multiple_is_sized_conservatively() {
        let kind_only = serde_json::json!({ "organization": { "organization_type": "claude_max" } });
        assert_eq!(claude_plan_from_profile(&kind_only).as_deref(), Some("max"));
        assert_eq!(plan_window_cap(Provider::Claude, Some("max")), 2_200_000);

        let flags_only = serde_json::json!({ "account": { "has_claude_max": true } });
        assert_eq!(claude_plan_from_profile(&flags_only).as_deref(), Some("max"));
        let pro_flag = serde_json::json!({ "account": { "has_claude_pro": true, "has_claude_max": false } });
        assert_eq!(claude_plan_from_profile(&pro_flag).as_deref(), Some("pro"));
    }

    /// A tier string we have never seen must not be guessed at. Passing it
    /// through verbatim would let an unrelated substring decide the cap — a
    /// hypothetical `claude_promo_trial` matching `contains("pro")` and sizing
    /// a trial like a Pro subscription.
    #[test]
    fn an_unknown_tier_falls_through_rather_than_matching_by_accident() {
        let odd = serde_json::json!({
            "organization": { "rate_limit_tier": "something_new", "organization_type": "claude_max" }
        });
        assert_eq!(claude_plan_from_profile(&odd).as_deref(), Some("max"), "the coarser field decides");
        let nothing = serde_json::json!({ "organization": { "rate_limit_tier": "something_new" } });
        assert_eq!(claude_plan_from_profile(&nothing), None);
        assert_eq!(claude_plan_from_profile(&serde_json::json!({})), None);
    }

    /// The token exchange's response has the same `account`/`organization`
    /// shape, so it is worth reading before spending a request on the profile —
    /// on the accounts where it happens to be populated, that is one fewer call.
    #[test]
    fn a_token_response_carrying_the_same_fields_is_read_the_same_way() {
        let tokens = serde_json::json!({
            "access_token": "…", "expires_in": 3600,
            "account": { "email_address": "…", "uuid": "…" },
            "organization": { "uuid": "…", "rate_limit_tier": "default_claude_max_5x" }
        });
        assert_eq!(claude_plan_from_profile(&tokens).as_deref(), Some("max5"));
    }
}
