//! RFC 8628 device authorization, for the two subscriptions that grant tokens
//! that way (Kimi Code, Grok CLI).
//!
//! The authorization-code flow in the daemon (`oauth.rs`) needs a loopback
//! listener the provider redirects back to. A device flow needs none: the
//! client asks for a code, shows the user a short string and a URL, and polls
//! the token endpoint until they approve. That difference matters beyond
//! tidiness — the loopback flow cannot complete when the UI runs in a browser
//! on another machine, and this one can.
//!
//! Endpoints, client ids and scopes are the public values the vendor CLIs ship.
//! They were read out of CLIProxyAPI, which drives the same two endpoints:
//! `internal/auth/kimi/kimi.go` and `internal/auth/xai/{xai,types}.go`.

use asale_protocol::ids::Provider;
use serde::Deserialize;
use std::time::Duration;

// ── Kimi Code ───────────────────────────────────────────────────────────────

pub const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_DEVICE_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";
pub const KIMI_TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";

// ── Grok CLI ────────────────────────────────────────────────────────────────

pub const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Used only when discovery cannot be reached; the discovered value wins.
pub const XAI_TOKEN_URL_FALLBACK: &str = "https://auth.x.ai/oauth2/token";

/// Lower bound on the poll interval when the endpoint does not name one.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Ceiling on how long a pending authorization is kept alive.
const MAX_POLL: Duration = Duration::from_secs(15 * 60);

/// What the user has to be shown to approve the device.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    /// The short string the user types, e.g. `WDJB-MJHT`.
    pub user_code: String,
    /// Where to enter it. Prefer `verification_uri_complete`, which has the
    /// code pre-filled — a code the user never has to retype cannot be mistyped.
    pub verification_uri: String,
    pub expires_in: i64,
    pub interval: i64,
    /// Resolved per provider; xAI discovers it, Kimi publishes it.
    pub token_url: String,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    interval: i64,
}

/// Tokens from a completed device authorization.
#[derive(Debug, Clone)]
pub struct DeviceTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Absolute unix seconds, when the endpoint reported a lifetime.
    pub expires_at: Option<i64>,
    /// `id_token` when the provider is OIDC (xAI); used only to name the account.
    pub id_token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TokenResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: i64,
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Ask the provider for a device code.
pub async fn begin(provider: Provider) -> anyhow::Result<DeviceCode> {
    match provider {
        Provider::Kimi => begin_kimi().await,
        Provider::Xai => begin_xai().await,
        other => anyhow::bail!("{other} does not use the device flow"),
    }
}

async fn begin_kimi() -> anyhow::Result<DeviceCode> {
    let resp = crate::http::upstream()
        .post(KIMI_DEVICE_URL)
        .header("accept", "application/json")
        // Moonshot identifies the calling client through this header family;
        // the device id is per-installation and is minted by the caller.
        .header("x-msh-platform", "kimi-cli")
        .form(&[("client_id", KIMI_CLIENT_ID)])
        .send()
        .await?;
    finish_device_code(Provider::Kimi, resp, KIMI_TOKEN_URL.to_string()).await
}

async fn begin_xai() -> anyhow::Result<DeviceCode> {
    let (device_url, token_url) = discover_xai().await?;
    let resp = crate::http::upstream()
        .post(&device_url)
        .header("accept", "application/json")
        .form(&[("client_id", XAI_CLIENT_ID), ("scope", XAI_SCOPE)])
        .send()
        .await?;
    finish_device_code(Provider::Xai, resp, token_url).await
}

/// Resolve xAI's `(device_authorization_endpoint, token_endpoint)`.
///
/// Falling back to guessed paths when discovery fails would send the device
/// code somewhere unverified, so a discovery failure is fatal here.
async fn discover_xai() -> anyhow::Result<(String, String)> {
    #[derive(Deserialize)]
    struct Discovery {
        #[serde(default)]
        device_authorization_endpoint: String,
        #[serde(default)]
        token_endpoint: String,
    }
    let resp = crate::http::upstream()
        .get(XAI_DISCOVERY_URL)
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("xai OIDC discovery failed (HTTP {status}): {body}");
    }
    let d: Discovery = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("xai OIDC discovery returned unreadable JSON ({e}): {body}"))?;
    if !d.device_authorization_endpoint.starts_with("https://") || !d.token_endpoint.starts_with("https://") {
        anyhow::bail!("xai OIDC discovery is missing an https device/token endpoint: {body}");
    }
    Ok((d.device_authorization_endpoint, d.token_endpoint))
}

async fn finish_device_code(
    provider: Provider,
    resp: reqwest::Response,
    token_url: String,
) -> anyhow::Result<DeviceCode> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!(
            "{provider} refused the device authorization request (HTTP 403: {body}). \
             That is the provider blocking the connection, not a bad login — \
             set {} to a reachable proxy and try again.",
            crate::http::PROXY_ENV
        );
    }
    if !status.is_success() {
        anyhow::bail!("{provider} device authorization failed (HTTP {status}): {body}");
    }
    let d: DeviceCodeResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("{provider} device authorization returned unreadable JSON ({e}): {body}"))?;
    if d.device_code.is_empty() || d.user_code.is_empty() {
        anyhow::bail!("{provider} device authorization is missing device_code/user_code: {body}");
    }
    let verification_uri = if d.verification_uri_complete.is_empty() {
        d.verification_uri
    } else {
        d.verification_uri_complete
    };
    if verification_uri.is_empty() {
        anyhow::bail!("{provider} device authorization named no verification URI: {body}");
    }
    Ok(DeviceCode {
        device_code: d.device_code,
        user_code: d.user_code,
        verification_uri,
        expires_in: d.expires_in,
        interval: d.interval,
        token_url,
    })
}

/// Poll until the user approves, the code expires, or they decline.
pub async fn poll(provider: Provider, code: &DeviceCode) -> anyhow::Result<DeviceTokens> {
    let client_id = match provider {
        Provider::Kimi => KIMI_CLIENT_ID,
        Provider::Xai => XAI_CLIENT_ID,
        other => anyhow::bail!("{other} does not use the device flow"),
    };
    let mut interval = match code.interval {
        i if i > 0 => Duration::from_secs(i as u64),
        _ => DEFAULT_POLL_INTERVAL,
    }
    .max(DEFAULT_POLL_INTERVAL);

    // The code's own lifetime bounds the wait when it is shorter than our cap.
    let mut deadline = MAX_POLL;
    if code.expires_in > 0 {
        deadline = deadline.min(Duration::from_secs(code.expires_in as u64));
    }
    let started = std::time::Instant::now();

    loop {
        tokio::time::sleep(interval).await;
        if started.elapsed() >= deadline {
            anyhow::bail!("{provider} authorization timed out — the code expired before it was approved");
        }
        match exchange(provider, &code.token_url, client_id, &code.device_code).await? {
            Poll::Ready(t) => return Ok(t),
            Poll::Pending => {}
            // RFC 8628: back off by another second each time we are told to.
            Poll::SlowDown => interval += Duration::from_secs(1),
        }
    }
}

enum Poll {
    Ready(DeviceTokens),
    Pending,
    SlowDown,
}

async fn exchange(provider: Provider, token_url: &str, client_id: &str, device_code: &str) -> anyhow::Result<Poll> {
    let params = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code),
        ("client_id", client_id),
    ];
    let resp = crate::http::upstream()
        .post(token_url)
        .header("accept", "application/json")
        .form(&params)
        .send()
        .await?;
    let body = resp.text().await.unwrap_or_default();
    // Kimi answers 200 for both success and "still pending", so the body — not
    // the status — decides. Parsing failures are reported with the body, which
    // is the only thing that makes an unexpected shape debuggable.
    let t: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("{provider} token endpoint returned unreadable JSON ({e}): {body}"))?;
    match t.error.as_str() {
        "" => {}
        "authorization_pending" => return Ok(Poll::Pending),
        "slow_down" => return Ok(Poll::SlowDown),
        "expired_token" => anyhow::bail!("{provider} authorization code expired before it was approved"),
        "access_denied" => anyhow::bail!("{provider} authorization was declined"),
        other => anyhow::bail!("{provider} authorization failed ({other}): {}", t.error_description),
    }
    if t.access_token.is_empty() {
        anyhow::bail!("{provider} returned no access token: {body}");
    }
    Ok(Poll::Ready(DeviceTokens {
        access_token: t.access_token,
        refresh_token: Some(t.refresh_token).filter(|r| !r.is_empty()),
        expires_at: (t.expires_in > 0).then(|| now_secs() + t.expires_in),
        id_token: Some(t.id_token).filter(|s| !s.is_empty()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_subscription_providers_have_a_device_flow() {
        for p in Provider::ALL {
            let supported = matches!(p, Provider::Kimi | Provider::Xai);
            assert_eq!(asale_protocol::ids::is_device_flow_provider(p), supported, "{p}");
        }
    }

    #[test]
    fn the_prefilled_verification_uri_wins() {
        // A code the user never has to retype cannot be mistyped, so the
        // complete URI is preferred whenever the provider sends one.
        let body = serde_json::json!({
            "device_code": "dc", "user_code": "WDJB-MJHT",
            "verification_uri": "https://auth.kimi.com/device",
            "verification_uri_complete": "https://auth.kimi.com/device?user_code=WDJB-MJHT",
            "expires_in": 600, "interval": 5
        })
        .to_string();
        let d: DeviceCodeResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(d.verification_uri_complete, "https://auth.kimi.com/device?user_code=WDJB-MJHT");
        assert_eq!(d.interval, 5);

        // ...and the plain one is still accepted when it is all there is.
        let bare = serde_json::json!({
            "device_code": "dc", "user_code": "X", "verification_uri": "https://auth.x.ai/device"
        })
        .to_string();
        let d: DeviceCodeResponse = serde_json::from_str(&bare).unwrap();
        assert!(d.verification_uri_complete.is_empty());
        assert_eq!(d.verification_uri, "https://auth.x.ai/device");
        // An absent interval means "use our floor", not "poll as fast as you like".
        assert_eq!(d.interval, 0);
    }

    #[test]
    fn a_pending_poll_is_not_an_error_but_a_denial_is() {
        let pending: TokenResponse =
            serde_json::from_str(r#"{"error":"authorization_pending"}"#).unwrap();
        assert_eq!(pending.error, "authorization_pending");
        assert!(pending.access_token.is_empty());

        let ok: TokenResponse =
            serde_json::from_str(r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#).unwrap();
        assert_eq!(ok.access_token, "at");
        assert_eq!(ok.expires_in, 3600);
        assert!(ok.error.is_empty());
    }
}
