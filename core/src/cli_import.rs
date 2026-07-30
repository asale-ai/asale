//! CLI credential import (spec §3.3, cc-switch style). Pure parsers for the
//! credential files each vendor CLI writes locally; the Tauri layer does the
//! filesystem / macOS-keychain reads and hands the raw JSON here.
//!
//! Sources (paths + JSON shapes verified against cc-switch
//! `src-tauri/src/services/subscription.rs`):
//!   - Claude Code: macOS Keychain service "Claude Code-credentials", or
//!     `~/.claude/.credentials.json` →
//!     `{"claudeAiOauth":{"accessToken","refreshToken","expiresAt"(ms),"subscriptionType"}}`
//!     (legacy key `"claude.ai_oauth"` also accepted).
//!   - Codex: `~/.codex/auth.json` →
//!     `{"OPENAI_API_KEY", "tokens":{"id_token","access_token","refresh_token","account_id"},"last_refresh"}`
//!     (macOS Keychain service "Codex Auth" holds the same JSON).
//!   - Gemini (gemini-cli): `~/.gemini/oauth_creds.json` →
//!     `{"access_token","refresh_token","id_token","expiry_date"(ms)}`
//!     (macOS Keychain "gemini-cli-oauth"/"main-account" uses
//!     `{"token":{"accessToken","refreshToken","expiresAt"(ms)}}`).

use base64::Engine;
use serde_json::Value;

/// A normalized credential extracted from a CLI's local storage.
#[derive(Debug, Clone, PartialEq)]
pub struct CliCred {
    /// asale provider id: claude | codex | gemini.
    pub provider: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds, when derivable.
    pub expires_at: Option<i64>,
    /// Email / account id hint when present (JWT claims etc.).
    pub account_hint: Option<String>,
    /// Plan hint (Claude `subscriptionType`, Codex `chatgpt_plan_type`).
    pub plan: Option<String>,
    /// The id the *vendor* knows this subscription by, when it has to travel
    /// with the token to be accepted.
    ///
    /// Only Codex uses it so far: the ChatGPT backend requires a
    /// `chatgpt-account-id` header alongside the bearer, and a request without
    /// it is a 401 that looks exactly like a revoked login. It lives here
    /// because `account_hint` is the *display* identity (an email) and is free
    /// to change or be absent; this one is a protocol value.
    pub upstream_account_id: Option<String>,
}

/// Normalize a timestamp that may be in milliseconds to seconds.
fn norm_ts_secs(v: i64) -> i64 {
    if v > 1_000_000_000_000 {
        v / 1000
    } else {
        v
    }
}

/// Decode the (unverified) claims of a JWT. Used only to extract display
/// metadata (email, plan, exp) from tokens the local CLI already holds.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Parse Claude Code credentials JSON (keychain and file share the shape).
pub fn parse_claude_credentials(content: &str) -> anyhow::Result<CliCred> {
    let v: Value = serde_json::from_str(content.trim())?;
    let entry = v
        .get("claudeAiOauth")
        .or_else(|| v.get("claude.ai_oauth"))
        .ok_or_else(|| anyhow::anyhow!("no claudeAiOauth entry in credentials"))?;
    let access = entry
        .get("accessToken")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("accessToken is empty or missing"))?;
    let refresh = entry
        .get("refreshToken")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from);
    let expires_at = entry.get("expiresAt").and_then(|e| e.as_i64()).map(norm_ts_secs);
    let plan = entry
        .get("subscriptionType")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(String::from);
    Ok(CliCred {
        provider: "claude".into(),
        access_token: access.to_string(),
        refresh_token: refresh,
        expires_at,
        account_hint: None, // the file carries no email; resolved via profile API
        plan,
        upstream_account_id: None,
    })
}

/// Parse Codex `auth.json`. Prefers the OAuth token set; falls back to the
/// plain `OPENAI_API_KEY` entry (API-key mode, no refresh/expiry).
pub fn parse_codex_auth(content: &str) -> anyhow::Result<CliCred> {
    let v: Value = serde_json::from_str(content.trim())?;
    if let Some(tokens) = v.get("tokens").filter(|t| !t.is_null()) {
        let access = tokens
            .get("access_token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("tokens.access_token is empty or missing"))?;
        let refresh = tokens
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(String::from);
        // The access token is a JWT — its `exp` claim is the real expiry.
        let expires_at = jwt_claims(access).and_then(|c| c.get("exp").and_then(|e| e.as_i64()));
        // Email and plan live in the id_token claims.
        let id_claims = tokens.get("id_token").and_then(|t| t.as_str()).and_then(jwt_claims);
        let email = id_claims
            .as_ref()
            .and_then(|c| c.get("email").and_then(|e| e.as_str()).map(String::from))
            .or_else(|| tokens.get("account_id").and_then(|a| a.as_str()).map(String::from));
        let auth_claim = id_claims.as_ref().and_then(|c| c.get("https://api.openai.com/auth"));
        let plan = auth_claim
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(|p| p.as_str())
            .map(String::from);
        // The ChatGPT backend will not serve this token without the account id
        // that goes with it. It rides in the same claim as the plan, and the
        // top-level `tokens.account_id` is the CLI's own copy of it — take
        // whichever is there, because an older auth.json has only the latter.
        let upstream_account_id = auth_claim
            .and_then(|a| a.get("chatgpt_account_id"))
            .and_then(|a| a.as_str())
            .or_else(|| tokens.get("account_id").and_then(|a| a.as_str()))
            .filter(|a| !a.is_empty())
            .map(String::from);
        return Ok(CliCred {
            provider: "codex".into(),
            access_token: access.to_string(),
            refresh_token: refresh,
            expires_at,
            account_hint: email,
            plan,
            upstream_account_id,
        });
    }
    let api_key = v
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow::anyhow!("auth.json has neither tokens nor OPENAI_API_KEY"))?;
    Ok(CliCred {
        provider: "codex".into(),
        access_token: api_key.to_string(),
        refresh_token: None,
        expires_at: None,
        account_hint: Some("api-key".into()),
        plan: None,
        upstream_account_id: None,
    })
}

/// Parse gemini-cli `oauth_creds.json` (flat file format) or the keytar
/// keychain format `{"token":{"accessToken",...}}`.
pub fn parse_gemini_oauth_creds(content: &str) -> anyhow::Result<CliCred> {
    let v: Value = serde_json::from_str(content.trim())?;
    // Keychain (keytar) nesting.
    if let Some(token) = v.get("token") {
        let access = token
            .get("accessToken")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("token.accessToken is empty or missing"))?;
        let refresh = token
            .get("refreshToken")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(String::from);
        let expires_at = token.get("expiresAt").and_then(|e| e.as_i64()).map(norm_ts_secs);
        return Ok(CliCred {
            provider: "gemini".into(),
            access_token: access.to_string(),
            refresh_token: refresh,
            expires_at,
            account_hint: None,
            plan: None,
            upstream_account_id: None,
        });
    }
    let access = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("access_token is empty or missing"))?;
    let refresh = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from);
    let expires_at = v.get("expiry_date").and_then(|e| e.as_i64()).map(norm_ts_secs);
    let email = v
        .get("id_token")
        .and_then(|t| t.as_str())
        .and_then(jwt_claims)
        .and_then(|c| c.get("email").and_then(|e| e.as_str()).map(String::from));
    Ok(CliCred {
        provider: "gemini".into(),
        access_token: access.to_string(),
        refresh_token: refresh,
        expires_at,
        account_hint: email,
        plan: None,
        upstream_account_id: None,
    })
}

// ── Writing a refreshed token back into the CLI's own file ──────────────────
//
// An `origin = "import"` account is not a copy of the CLI's login, it is the
// same login: one OAuth identity, one refresh token. Anthropic and OpenAI both
// rotate that token on use — the old one dies the moment either side redeems
// it. So a refresh performed by asale silently invalidates what is sitting in
// `~/.claude/.credentials.json`, and the next time the user opens their own
// Claude Code it is logged out, with nothing on screen connecting that to the
// marketplace app they left running.
//
// Hence these: after refreshing a shared credential, put the new one back where
// the CLI will look for it. Pure string→string so the shapes stay beside the
// parsers that read them; the caller does the (atomic) file write.

/// A freshly refreshed token set, on its way back into a vendor CLI's file.
#[derive(Debug, Clone, Copy)]
pub struct RefreshedCred<'a> {
    pub access_token: &'a str,
    /// `None` when the provider does not rotate refresh tokens (Google).
    pub refresh_token: Option<&'a str>,
    /// Absolute unix seconds, when the provider says so.
    pub expires_at: Option<i64>,
    /// Now, in unix seconds — Codex records when it last refreshed.
    pub now_secs: i64,
}

/// Rewrite `raw` (the CLI's own credential file) with `cred`, leaving every
/// other field exactly as it was.
///
/// Errors when the file is not the shape this provider's CLI writes, which is
/// the case worth refusing: a half-understood file is one asale should hand
/// back untouched rather than overwrite with a guess.
pub fn patch_cli_credentials(provider: &str, raw: &str, cred: RefreshedCred<'_>) -> anyhow::Result<String> {
    let mut v: Value = serde_json::from_str(raw.trim())?;
    match provider {
        "claude" => {
            // The legacy key spelling is accepted on read, so it has to be
            // accepted on write too — rewriting under the modern name would
            // leave the CLI reading the old, now-dead entry.
            let key = ["claudeAiOauth", "claude.ai_oauth"]
                .into_iter()
                .find(|k| v.get(*k).is_some())
                .ok_or_else(|| anyhow::anyhow!("no claudeAiOauth entry to update"))?;
            let entry = v
                .get_mut(key)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow::anyhow!("{key} is not an object"))?;
            entry.insert("accessToken".into(), Value::String(cred.access_token.into()));
            if let Some(r) = cred.refresh_token {
                entry.insert("refreshToken".into(), Value::String(r.into()));
            }
            if let Some(e) = cred.expires_at {
                entry.insert("expiresAt".into(), Value::from(e * 1000));
            }
        }
        "codex" => {
            let tokens = v
                .get_mut("tokens")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow::anyhow!("auth.json has no OAuth token set to update"))?;
            tokens.insert("access_token".into(), Value::String(cred.access_token.into()));
            if let Some(r) = cred.refresh_token {
                tokens.insert("refresh_token".into(), Value::String(r.into()));
            }
            // Codex decides whether to refresh from this timestamp. Left at the
            // old value it would refresh on its own next launch and rotate
            // *asale* out — the same bug pointed the other way.
            if let Some(obj) = v.as_object_mut() {
                obj.insert("last_refresh".into(), Value::String(rfc3339_utc(cred.now_secs)));
            }
        }
        "gemini" => {
            // Keytar nesting vs. the flat file; both are shapes we read.
            if let Some(token) = v.get_mut("token").and_then(Value::as_object_mut) {
                token.insert("accessToken".into(), Value::String(cred.access_token.into()));
                if let Some(r) = cred.refresh_token {
                    token.insert("refreshToken".into(), Value::String(r.into()));
                }
                if let Some(e) = cred.expires_at {
                    token.insert("expiresAt".into(), Value::from(e * 1000));
                }
            } else {
                let obj = v.as_object_mut().ok_or_else(|| anyhow::anyhow!("oauth_creds.json is not an object"))?;
                obj.insert("access_token".into(), Value::String(cred.access_token.into()));
                if let Some(r) = cred.refresh_token {
                    obj.insert("refresh_token".into(), Value::String(r.into()));
                }
                if let Some(e) = cred.expires_at {
                    obj.insert("expiry_date".into(), Value::from(e * 1000));
                }
            }
        }
        other => anyhow::bail!("no credential file shape known for {other}"),
    }
    Ok(serde_json::to_string_pretty(&v)?)
}

/// `2026-07-30T05:30:08Z` from unix seconds. Codex writes `last_refresh` in
/// this form and there is no date crate in this dependency tree for one field.
fn rfc3339_utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // days since 1970-01-01 → civil date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Build a credential from a raw API key (Moonshot / xAI).
///
/// There is no file to parse and no OAuth exchange to run: the key the user
/// pastes is the whole credential. It is stored with no refresh token and no
/// expiry, which is what keeps the refresh loop away from it — an API key does
/// not expire, and `needs_refresh(None, ..)` is false.
///
/// The paste is cleaned up before it is stored: a key copied out of a dashboard
/// or lifted from a shell export arrives with a trailing newline or wrapping
/// quotes, and either one fails upstream as an opaque 401 that looks like a
/// revoked key rather than a typo.
pub fn api_key_cred(provider: &str, key: &str) -> anyhow::Result<CliCred> {
    let key = key.trim().trim_matches(['"', '\'']).trim();
    if key.is_empty() {
        anyhow::bail!("API key is empty");
    }
    if key.chars().any(char::is_whitespace) {
        anyhow::bail!("API key contains whitespace — paste just the key, not the whole command");
    }
    Ok(CliCred {
        provider: provider.to_string(),
        access_token: key.to_string(),
        refresh_token: None,
        expires_at: None,
        account_hint: None,
        plan: None,
        upstream_account_id: None,
    })
}

// ── Account identity & de-duplication (spec §3.3) ───────────────────────────
//
// One subscription account is usually reachable through several local stores:
// Claude Code keeps the *same* OAuth token in both the macOS keychain and
// `~/.claude/.credentials.json`, and a refreshed copy may lag behind in one of
// them. They are one account, and selling is per account — so the sell side
// must end up with one record that lists every place the token was found,
// never one record per file.

/// A credential together with the place it was read from.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcedCred {
    pub cred: CliCred,
    /// `keychain:<service>` or a file path.
    pub source: String,
}

/// Short, non-reversible digest of a credential's most stable secret.
///
/// Used as the last-resort account identity when no email can be resolved. Two
/// stores holding the same token are certainly the same account; two different
/// logins get different fingerprints, so this never merges accounts the way a
/// fixed `"<provider>-cli"` placeholder would.
pub fn token_fingerprint(cred: &CliCred) -> String {
    use sha2::{Digest, Sha256};
    // Prefer the refresh token: it outlives access-token rotation, so an
    // account keeps one identity across a refresh by either side.
    let material = cred.refresh_token.as_deref().unwrap_or(cred.access_token.as_str());
    let digest = Sha256::digest(material.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// One subscription account, merged from every source that holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedAccount {
    pub account_id: String,
    /// The credential actually imported — the freshest of the group.
    pub cred: CliCred,
    /// The source that credential was read from (`sources[0]` is discovery
    /// order, this one is the *chosen* store and may differ).
    pub source: String,
    /// Every source holding this account, in discovery order.
    pub sources: Vec<String>,
}

/// Ranks a candidate within its account group; higher is better. Unexpired
/// beats expired, a refresh token beats none, then the later expiry wins.
fn rank(c: &CliCred, now: i64) -> (u8, u8, i64) {
    let unexpired = u8::from(c.expires_at.is_none_or(|e| e > now));
    let refreshable = u8::from(c.refresh_token.is_some());
    (unexpired, refreshable, c.expires_at.unwrap_or(0))
}

/// Collapse `(account_id, sourced credential)` pairs into one record per
/// account, keeping the freshest credential of each group and listing all of
/// its sources. Discovery order is preserved for both accounts and sources, so
/// the caller's source priority survives into the UI.
pub fn merge_by_account(items: Vec<(String, SourcedCred)>, now: i64) -> Vec<MergedAccount> {
    let mut out: Vec<MergedAccount> = Vec::new();
    for (account_id, sc) in items {
        match out.iter_mut().find(|m| m.account_id == account_id) {
            Some(m) => {
                if !m.sources.contains(&sc.source) {
                    m.sources.push(sc.source.clone());
                }
                // Strictly better only: a tie keeps the earlier (higher
                // priority) source as the one we actually import.
                if rank(&sc.cred, now) > rank(&m.cred, now) {
                    m.cred = sc.cred;
                    m.source = sc.source;
                }
            }
            None => out.push(MergedAccount {
                account_id,
                cred: sc.cred,
                sources: vec![sc.source.clone()],
                source: sc.source,
            }),
        }
    }
    out
}

/// Environment variables that override the given CLI's stored credentials
/// (spec §3.3 — cc-switch style conflict detection). Returns the names that
/// are currently set (non-empty) via `get`.
pub fn env_conflicts(provider: &str, get: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let names: &[&str] = match provider {
        "claude" | "claude_work" => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"],
        "codex" => &["OPENAI_API_KEY", "OPENAI_BASE_URL"],
        "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        _ => &[],
    };
    names
        .iter()
        .filter(|n| get(n).is_some_and(|v| !v.trim().is_empty()))
        .map(|n| n.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a fake (unsigned) JWT with the given claims.
    fn fake_jwt(claims: serde_json::Value) -> String {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!("{}.{}.{}", b64(b"{\"alg\":\"none\"}"), b64(claims.to_string().as_bytes()), b64(b"sig"))
    }

    #[test]
    fn claude_credentials_parse_both_key_names() {
        let modern = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-x","refreshToken":"sk-ant-ort01-y",
            "expiresAt":1750000000000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
        let c = parse_claude_credentials(modern).unwrap();
        assert_eq!(c.provider, "claude");
        assert_eq!(c.access_token, "sk-ant-oat01-x");
        assert_eq!(c.refresh_token.as_deref(), Some("sk-ant-ort01-y"));
        assert_eq!(c.expires_at, Some(1_750_000_000)); // ms → s
        assert_eq!(c.plan.as_deref(), Some("max"));

        let legacy = r#"{"claude.ai_oauth":{"accessToken":"a","expiresAt":1750000000}}"#;
        let c = parse_claude_credentials(legacy).unwrap();
        assert_eq!(c.access_token, "a");
        assert_eq!(c.expires_at, Some(1_750_000_000)); // already seconds
        assert!(c.refresh_token.is_none());
    }

    #[test]
    fn claude_credentials_reject_empty() {
        assert!(parse_claude_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_err());
        assert!(parse_claude_credentials(r#"{"other":1}"#).is_err());
        assert!(parse_claude_credentials("not json").is_err());
    }

    #[test]
    fn codex_auth_oauth_mode_extracts_email_plan_exp() {
        let id_token = fake_jwt(serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {"chatgpt_plan_type": "plus", "chatgpt_account_id": "acc-1"}
        }));
        let access = fake_jwt(serde_json::json!({"exp": 1_760_000_000i64}));
        let content = serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": id_token, "access_token": access, "refresh_token": "rt-1", "account_id": "acc-1"},
            "last_refresh": "2026-07-01T00:00:00Z"
        })
        .to_string();
        let c = parse_codex_auth(&content).unwrap();
        assert_eq!(c.provider, "codex");
        assert_eq!(c.account_hint.as_deref(), Some("dev@example.com"));
        assert_eq!(c.plan.as_deref(), Some("plus"));
        assert_eq!(c.expires_at, Some(1_760_000_000));
        assert_eq!(c.refresh_token.as_deref(), Some("rt-1"));
        // Without this the ChatGPT backend 401s every relayed call, which the
        // pool then reads as "this login is dead" and takes the account off the
        // market — so it has to survive the import, not just the file.
        assert_eq!(c.upstream_account_id.as_deref(), Some("acc-1"));
    }

    /// An auth.json written before the claim existed still carries the id at the
    /// top level of `tokens`, and that copy is just as usable.
    #[test]
    fn codex_account_id_falls_back_to_the_tokens_entry() {
        let id_token = fake_jwt(serde_json::json!({"email": "dev@example.com"}));
        let access = fake_jwt(serde_json::json!({"exp": 1_760_000_000i64}));
        let content = serde_json::json!({
            "tokens": {"id_token": id_token, "access_token": access, "account_id": "acc-legacy"}
        })
        .to_string();
        assert_eq!(
            parse_codex_auth(&content).unwrap().upstream_account_id.as_deref(),
            Some("acc-legacy")
        );
    }

    #[test]
    fn codex_auth_api_key_fallback() {
        let c = parse_codex_auth(r#"{"OPENAI_API_KEY":"sk-test"}"#).unwrap();
        assert_eq!(c.access_token, "sk-test");
        assert!(c.refresh_token.is_none());
        assert_eq!(c.account_hint.as_deref(), Some("api-key"));
        assert!(parse_codex_auth(r#"{}"#).is_err());
    }

    #[test]
    fn gemini_creds_file_and_keychain_formats() {
        let id_token = fake_jwt(serde_json::json!({"email": "g@example.com"}));
        let file = serde_json::json!({
            "access_token": "ya29.x", "refresh_token": "1//r", "id_token": id_token,
            "expiry_date": 1_750_000_000_000i64
        })
        .to_string();
        let c = parse_gemini_oauth_creds(&file).unwrap();
        assert_eq!(c.provider, "gemini");
        assert_eq!(c.account_hint.as_deref(), Some("g@example.com"));
        assert_eq!(c.expires_at, Some(1_750_000_000));

        let keychain = r#"{"token":{"accessToken":"ya29.k","refreshToken":"1//k","expiresAt":1750000000000},"updatedAt":1}"#;
        let c = parse_gemini_oauth_creds(keychain).unwrap();
        assert_eq!(c.access_token, "ya29.k");
        assert_eq!(c.refresh_token.as_deref(), Some("1//k"));
        assert_eq!(c.expires_at, Some(1_750_000_000));
    }

    fn cred(access: &str, refresh: Option<&str>, exp: Option<i64>) -> CliCred {
        CliCred {
            provider: "claude".into(),
            access_token: access.into(),
            refresh_token: refresh.map(String::from),
            expires_at: exp,
            account_hint: None,
            plan: Some("max".into()),
            upstream_account_id: None,
        }
    }

    #[test]
    fn same_account_from_two_stores_becomes_one_record() {
        let now = 1_000_000i64;
        // Keychain holds a refreshed copy; the file lagged behind with an older
        // access token — same login, so one record listing both stores.
        let items = vec![
            (
                "me@x.com".to_string(),
                SourcedCred {
                    cred: cred("newer", Some("rt"), Some(now + 3600)),
                    source: "keychain:Claude Code-credentials".into(),
                },
            ),
            (
                "me@x.com".to_string(),
                SourcedCred { cred: cred("older", Some("rt"), Some(now - 10)), source: "~/.claude/.credentials.json".into() },
            ),
        ];
        let merged = merge_by_account(items, now);
        assert_eq!(merged.len(), 1, "one subscription account → one record");
        assert_eq!(merged[0].sources.len(), 2, "both stores listed as sources");
        assert_eq!(merged[0].cred.access_token, "newer", "freshest credential imported");
        assert_eq!(merged[0].source, "keychain:Claude Code-credentials");
    }

    #[test]
    fn an_expired_keychain_copy_yields_to_a_live_file_copy() {
        let now = 1_000_000i64;
        let items = vec![
            ("me@x.com".to_string(), SourcedCred { cred: cred("dead", Some("rt"), Some(now - 1)), source: "keychain:X".into() }),
            ("me@x.com".to_string(), SourcedCred { cred: cred("live", Some("rt"), Some(now + 60)), source: "file".into() }),
        ];
        let merged = merge_by_account(items, now);
        assert_eq!(merged[0].cred.access_token, "live");
        assert_eq!(merged[0].source, "file");
        assert_eq!(merged[0].sources, vec!["keychain:X".to_string(), "file".to_string()]);
    }

    #[test]
    fn different_accounts_stay_separate() {
        let now = 1_000_000i64;
        let items = vec![
            ("a@x.com".to_string(), SourcedCred { cred: cred("t1", Some("r1"), None), source: "keychain:X".into() }),
            ("b@x.com".to_string(), SourcedCred { cred: cred("t2", Some("r2"), None), source: "file".into() }),
        ];
        let merged = merge_by_account(items, now);
        assert_eq!(merged.len(), 2, "two logins are never collapsed into one row");
    }

    #[test]
    fn fingerprint_follows_the_refresh_token_not_the_access_token() {
        // The access token rotates on every refresh; the identity must not.
        let a = cred("access-1", Some("refresh"), None);
        let b = cred("access-2", Some("refresh"), None);
        assert_eq!(token_fingerprint(&a), token_fingerprint(&b));
        let other = cred("access-1", Some("other-refresh"), None);
        assert_ne!(token_fingerprint(&a), token_fingerprint(&other));
        // No refresh token → falls back to the access token, still stable.
        let no_refresh = cred("access-1", None, None);
        assert_eq!(token_fingerprint(&no_refresh), token_fingerprint(&cred("access-1", None, None)));
    }

    #[test]
    fn api_key_cred_trims_and_rejects_junk() {
        let c = api_key_cred("kimi", "  sk-moon-1\n").unwrap();
        assert_eq!(c.access_token, "sk-moon-1");
        assert!(c.refresh_token.is_none());
        // No expiry is the load-bearing part: it is what keeps the refresh loop
        // from trying to renew a key that has nothing to renew.
        assert!(c.expires_at.is_none());
        assert_eq!(api_key_cred("xai", "\"xai-key\" ").unwrap().access_token, "xai-key");
        assert!(api_key_cred("kimi", "   ").is_err());
        assert!(api_key_cred("kimi", "\"\"").is_err());
        assert!(
            api_key_cred("xai", "export XAI_API_KEY=abc").is_err(),
            "a whole shell line is not a key"
        );
    }

    #[test]
    fn env_conflicts_flags_only_set_vars() {
        let envs = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk-x".to_string()),
            "ANTHROPIC_AUTH_TOKEN" => Some("  ".to_string()), // blank → not a conflict
            _ => None,
        };
        let c = env_conflicts("claude", envs);
        assert_eq!(c, vec!["ANTHROPIC_API_KEY".to_string()]);
        assert!(env_conflicts("codex", |_| None).is_empty());
        assert!(env_conflicts("unknown", |_| Some("x".into())).is_empty());
    }

    // ── write-back ──────────────────────────────────────────────────────

    fn fresh<'a>(access: &'a str, refresh: Option<&'a str>) -> RefreshedCred<'a> {
        RefreshedCred { access_token: access, refresh_token: refresh, expires_at: Some(1_785_403_714), now_secs: 1_785_392_054 }
    }

    #[test]
    fn claude_write_back_replaces_the_tokens_and_keeps_the_rest() {
        let original = r#"{"claudeAiOauth":{"accessToken":"old-a","refreshToken":"old-r","expiresAt":1000,"subscriptionType":"max","scopes":["user:inference"]}}"#;
        let out = patch_cli_credentials("claude", original, fresh("new-a", Some("new-r"))).unwrap();

        // The CLI must be able to read back exactly what asale now holds —
        // that is the whole point of writing at all.
        let c = parse_claude_credentials(&out).unwrap();
        assert_eq!(c.access_token, "new-a");
        assert_eq!(c.refresh_token.as_deref(), Some("new-r"));
        assert_eq!(c.expires_at, Some(1_785_403_714), "expiresAt round-trips through milliseconds");
        assert_eq!(c.plan.as_deref(), Some("max"), "untouched fields survive");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["claudeAiOauth"]["scopes"][0], "user:inference");
    }

    #[test]
    fn claude_write_back_keeps_the_legacy_key_spelling() {
        let original = r#"{"claude.ai_oauth":{"accessToken":"old-a","refreshToken":"old-r"}}"#;
        let out = patch_cli_credentials("claude", original, fresh("new-a", Some("new-r"))).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["claude.ai_oauth"]["accessToken"], "new-a");
        assert!(v.get("claudeAiOauth").is_none(), "writing under the modern name would leave the CLI on the dead entry");
    }

    #[test]
    fn codex_write_back_updates_tokens_and_the_refresh_stamp() {
        let original = r#"{"OPENAI_API_KEY":null,"tokens":{"id_token":"idt","access_token":"old-a","refresh_token":"old-r","account_id":"acc-1"},"last_refresh":"2026-07-01T00:00:00Z"}"#;
        let out = patch_cli_credentials("codex", original, fresh("new-a", Some("new-r"))).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["tokens"]["access_token"], "new-a");
        assert_eq!(v["tokens"]["refresh_token"], "new-r");
        assert_eq!(v["tokens"]["account_id"], "acc-1", "the id the backend requires is not ours to drop");
        assert_eq!(v["last_refresh"], "2026-07-30T06:14:14Z", "left stale, codex refreshes on its own and rotates us out");
    }

    #[test]
    fn gemini_write_back_handles_both_shapes() {
        let flat = r#"{"access_token":"old-a","refresh_token":"old-r","expiry_date":1000,"id_token":"idt"}"#;
        let out = patch_cli_credentials("gemini", flat, fresh("new-a", Some("new-r"))).unwrap();
        let c = parse_gemini_oauth_creds(&out).unwrap();
        assert_eq!(c.access_token, "new-a");
        assert_eq!(c.expires_at, Some(1_785_403_714));

        let nested = r#"{"token":{"accessToken":"old-a","refreshToken":"old-r","expiresAt":1000}}"#;
        let out = patch_cli_credentials("gemini", nested, fresh("new-a", None)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token"]["accessToken"], "new-a");
        assert_eq!(v["token"]["refreshToken"], "old-r", "a provider that does not rotate keeps its refresh token");
    }

    #[test]
    fn write_back_refuses_a_file_it_does_not_recognize() {
        assert!(patch_cli_credentials("claude", r#"{"something":"else"}"#, fresh("a", None)).is_err());
        assert!(patch_cli_credentials("codex", r#"{"OPENAI_API_KEY":"sk-x"}"#, fresh("a", None)).is_err(), "api-key mode has no token set");
        assert!(patch_cli_credentials("claude", "not json", fresh("a", None)).is_err());
        assert!(patch_cli_credentials("kimi", "{}", fresh("a", None)).is_err());
    }

    #[test]
    fn rfc3339_matches_the_calendar() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_785_392_054), "2026-07-30T06:14:14Z");
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00Z", "leap day");
    }
}
