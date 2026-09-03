//! Local CLI credential scanning (spec §3.3). Finds credentials the installed
//! vendor CLIs already hold and normalizes them via `asale_client_core::cli_import`.
//!
//! Sources (verified against cc-switch `services/subscription.rs`):
//!   - Claude Code: macOS Keychain service "Claude Code-credentials" (via
//!     `security find-generic-password -w`), then `~/.claude/.credentials.json`.
//!   - Codex: macOS Keychain service "Codex Auth", then `~/.codex/auth.json`.
//!   - Gemini (gemini-cli): macOS Keychain "gemini-cli-oauth"/"main-account",
//!     then `~/.gemini/oauth_creds.json`.

use asale_client_core::cli_import::{self, CliCred, SourcedCred};
use serde::Serialize;

/// A discovered (not yet imported) credential source.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredCred {
    pub provider: String,
    /// Where it was found: `keychain:<service>` or the file path.
    pub source: String,
    pub has_refresh_token: bool,
    pub expires_at: Option<i64>,
    pub expired: bool,
    /// Digest of this credential's refresh token. Two sources sharing a
    /// fingerprint are the same subscription account — that is how the scan can
    /// tell "keychain and file" apart from "two different logins" without
    /// calling any upstream profile endpoint.
    pub fingerprint: String,
    pub account_hint: Option<String>,
    pub plan: Option<String>,
}

fn home() -> String {
    crate::state::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| ".".into())
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Read a generic password from the macOS keychain via `security`(1) — these
/// entries belong to other apps, so the portable `keyring` crate (scoped to our
/// own service) cannot read them.
#[cfg(target_os = "macos")]
fn read_os_keychain(service: &str, account: Option<&str>) -> Option<String> {
    // `$HOME` can be redirected at a sandbox, the login keychain cannot: the
    // integration tests set this so a developer's real credentials never leak
    // into a run that is supposed to see only its throwaway home.
    if std::env::var("ASALE_DISABLE_OS_KEYCHAIN_SCAN").is_ok_and(|v| v == "1") {
        return None;
    }
    let mut args = vec!["find-generic-password", "-s", service];
    if let Some(a) = account {
        args.push("-a");
        args.push(a);
    }
    args.push("-w");
    let out = std::process::Command::new("security").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(not(target_os = "macos"))]
fn read_os_keychain(_service: &str, _account: Option<&str>) -> Option<String> {
    None
}

/// Where opencode keeps the logins it was signed into.
///
/// XDG data home on every platform, Windows included — `opencode debug paths`
/// on Windows 11 answers `C:\Users\<u>\.local\share\opencode`. See
/// `tool_config::opencode_config_dir` for the same story on the config side.
fn opencode_auth_path() -> String {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("{}/.local/share", home()));
    format!("{base}/opencode/auth.json")
}

/// How to read one credential store. Carried with the source rather than
/// derived from the provider: a provider can be reachable through more than one
/// tool's store, and those stores do not share a file format — opencode holds a
/// Claude login in a shape nothing else writes.
type Parser = fn(&str) -> anyhow::Result<CliCred>;

/// One provider's candidate sources, in import-priority order:
/// (label, loader, parser) — keychain first, then credential files.
fn sources(provider: &str) -> Vec<(String, Option<String>, Parser)> {
    let h = home();
    match provider {
        "claude" => vec![
            (
                "keychain:Claude Code-credentials".to_string(),
                read_os_keychain("Claude Code-credentials", None),
                cli_import::parse_claude_credentials as Parser,
            ),
            (
                format!("{h}/.claude/.credentials.json"),
                std::fs::read_to_string(format!("{h}/.claude/.credentials.json")).ok(),
                cli_import::parse_claude_credentials as Parser,
            ),
            // A Claude subscription signed into through opencode instead. It is
            // the same login, so when both stores hold it the two sources merge
            // by fingerprint into one account; when only this one does, the
            // subscription is still sellable rather than invisible.
            {
                let p = opencode_auth_path();
                let raw = std::fs::read_to_string(&p).ok();
                (p, raw, cli_import::parse_opencode_auth as Parser)
            },
        ],
        "codex" => vec![
            (
                "keychain:Codex Auth".to_string(),
                read_os_keychain("Codex Auth", None),
                cli_import::parse_codex_auth as Parser,
            ),
            (
                format!("{h}/.codex/auth.json"),
                std::fs::read_to_string(format!("{h}/.codex/auth.json")).ok(),
                cli_import::parse_codex_auth as Parser,
            ),
        ],
        "gemini" => vec![
            (
                "keychain:gemini-cli-oauth".to_string(),
                read_os_keychain("gemini-cli-oauth", Some("main-account")),
                cli_import::parse_gemini_oauth_creds as Parser,
            ),
            (
                format!("{h}/.gemini/oauth_creds.json"),
                std::fs::read_to_string(format!("{h}/.gemini/oauth_creds.json")).ok(),
                cli_import::parse_gemini_oauth_creds as Parser,
            ),
        ],
        _ => vec![],
    }
}

/// Scan all known CLIs; returns every parseable source (does not import).
pub fn scan() -> Vec<DiscoveredCred> {
    let now = now_secs();
    let mut found = Vec::new();
    for provider in crate::tool_config::TOOLS {
        for s in load_all(provider) {
            found.push(DiscoveredCred {
                provider: provider.to_string(),
                source: s.source,
                has_refresh_token: s.cred.refresh_token.is_some(),
                expires_at: s.cred.expires_at,
                expired: s.cred.expires_at.is_some_and(|e| e <= now),
                fingerprint: cli_import::token_fingerprint(&s.cred),
                account_hint: s.cred.account_hint,
                plan: s.cred.plan,
            });
        }
    }
    found
}

/// Load *every* parseable credential for one provider, in priority order.
///
/// Several stores usually hold the same login (Claude Code writes its token to
/// both the keychain and `~/.claude/.credentials.json`), and occasionally they
/// hold different ones. The caller resolves each credential's account identity
/// and merges by account, so the same subscription ends up as one record with
/// all of its sources listed — and a second, genuinely different account is
/// no longer silently dropped in favour of the first store scanned.
pub fn load_all(provider: &str) -> Vec<SourcedCred> {
    let mut out = Vec::new();
    for (source, content, parse) in sources(provider) {
        let Some(content) = content else { continue };
        match parse(&content) {
            Ok(cred) => out.push(SourcedCred { cred, source }),
            // A store that holds no login for *this* provider is the ordinary
            // case now that one file can hold several — opencode's `auth.json`
            // exists the moment you sign into anything with it. Debug, not warn.
            Err(e) => tracing::debug!(provider, source, "cli credential not usable: {e}"),
        }
    }
    out
}

/// Load the best (priority-ordered) credential for one provider.
pub fn load(provider: &str) -> anyhow::Result<(CliCred, String)> {
    load_all(provider)
        .into_iter()
        .next()
        .map(|s| (s.cred, s.source))
        .ok_or_else(|| anyhow::anyhow!("no {provider} CLI credentials found on this machine"))
}

/// Whether `source` is one of Claude Code's *own* stores — the keychain item
/// or `~/.claude/*` — as opposed to another tool (opencode) holding a Claude
/// login of its own. Only those are what `~/.claude.json` describes (C10).
pub fn is_claude_code_store(source: &str) -> bool {
    source == "keychain:Claude Code-credentials" || source.starts_with(&format!("{}/.claude/", home()))
}

/// The account the locally installed Claude Code CLI is signed in as, read from
/// `~/.claude.json`. The credential stores themselves carry no identity, so
/// this is the offline fallback when the profile endpoint can't be reached —
/// and it is only a *hint*: it names the CLI's current login, which is the one
/// both Claude stores hold in practice.
pub fn claude_local_account() -> Option<String> {
    let content = std::fs::read_to_string(format!("{}/.claude.json", home())).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.pointer("/oauthAccount/emailAddress")
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty())
        .map(String::from)
}

/// Resolve the Anthropic account email for an imported Claude token
/// (best-effort; the credential file itself carries no identity).
pub async fn claude_profile_email(access_token: &str) -> Option<String> {
    let resp = asale_client_core::http::upstream()
        .get("https://api.anthropic.com/api/oauth/profile")
        .header("authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.pointer("/account/email")
        .or_else(|| v.get("email"))
        .and_then(|e| e.as_str())
        .map(String::from)
}
