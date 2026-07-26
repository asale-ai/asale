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
        let plan = id_claims
            .as_ref()
            .and_then(|c| c.get("https://api.openai.com/auth"))
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(|p| p.as_str())
            .map(String::from);
        return Ok(CliCred {
            provider: "codex".into(),
            access_token: access.to_string(),
            refresh_token: refresh,
            expires_at,
            account_hint: email,
            plan,
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
}
