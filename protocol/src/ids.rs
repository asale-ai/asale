//! The identifier vocabulary both sides share.
//!
//! Three different things in this system were all called "provider", which is
//! why the same list of strings kept being written out by hand in a different
//! shape in each module:
//!
//! * [`Provider`] — an **upstream credential family**. What a publisher holds a
//!   subscription for and injects a token for: `claude`, `claude_work`,
//!   `codex`, `gemini`, `kimi`, `xai`. This is what travels on the wire.
//! * [`Vendor`] — a **catalog vendor slug**, the left half of an OpenRouter
//!   model id (`anthropic/claude-opus-4`) and the `prices.provider` column:
//!   `anthropic`, `openai`, `google`.
//! * a *tool* — a locally installed AI CLI whose config the buy switch rewrites
//!   (Claude Code, Codex, Gemini CLI). Purely client-side and never on the
//!   wire, so it lives in the daemon (`tool_config::Tool`), not here.

use serde::{Deserialize, Serialize};

/// Upstream token family a publisher injects credentials for.
///
/// `claude_work` shares the claude upstream URL and translator; only the
/// user-agent differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    ClaudeWork,
    Codex,
    Gemini,
    Kimi,
    Xai,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::ClaudeWork => "claude_work",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
            Provider::Kimi => "kimi",
            Provider::Xai => "xai",
        }
    }

    /// Parse a wire/database string. `None` for anything unknown — callers
    /// decide whether that is a rejection or a skip.
    pub fn from_str_opt(s: &str) -> Option<Provider> {
        Some(match s {
            "claude" => Provider::Claude,
            "claude_work" => Provider::ClaudeWork,
            "codex" => Provider::Codex,
            "gemini" => Provider::Gemini,
            "kimi" => Provider::Kimi,
            "xai" => Provider::Xai,
            _ => return None,
        })
    }

    /// The upstream family, which drives translator + URL choice. `claude_work`
    /// maps to the claude family (same upstream, different UA).
    pub fn upstream_family(&self) -> Provider {
        match self {
            Provider::ClaudeWork => Provider::Claude,
            other => *other,
        }
    }

    pub const ALL: [Provider; 6] = [
        Provider::Claude,
        Provider::ClaudeWork,
        Provider::Codex,
        Provider::Gemini,
        Provider::Kimi,
        Provider::Xai,
    ];
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Catalog vendor slug — the left half of an OpenRouter model id, stored in
/// `prices.provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Anthropic,
    Openai,
    Google,
}

impl Vendor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::Openai => "openai",
            Vendor::Google => "google",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Vendor> {
        Some(match s {
            "anthropic" => Vendor::Anthropic,
            "openai" => Vendor::Openai,
            "google" => Vendor::Google,
            _ => return None,
        })
    }

    /// The credential families whose subscriptions can serve this vendor's
    /// models.
    ///
    /// `Kimi` and `Xai` are intentionally unreachable here: no vendor slug maps
    /// to them, so no catalog row can currently be sold from one. They exist as
    /// `Provider` variants because the wire format and the translator already
    /// handle them — add the slug here when subscriptions for them ship.
    pub fn providers(&self) -> &'static [Provider] {
        match self {
            Vendor::Anthropic => &[Provider::Claude, Provider::ClaudeWork],
            Vendor::Openai => &[Provider::Codex],
            Vendor::Google => &[Provider::Gemini],
        }
    }

    pub const ALL: [Vendor; 3] = [Vendor::Anthropic, Vendor::Openai, Vendor::Google];
}

/// Every provider some catalog vendor maps to — i.e. the credential families a
/// subscription can actually be imported for and sold from.
///
/// This is the union of [`Vendor::providers`] over [`Vendor::ALL`], kept as a
/// constant because callers iterate it in a fixed display order. The test below
/// holds the two in sync.
pub const SUBSCRIBABLE_PROVIDERS: &[Provider] = &[
    Provider::Claude,
    Provider::ClaudeWork,
    Provider::Codex,
    Provider::Gemini,
];

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Usage token classes. Pricing is per class, so this is part of the usage
/// vocabulary rather than a server-internal detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenType {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl TokenType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenType::Input => "input",
            TokenType::Output => "output",
            TokenType::CacheRead => "cache_read",
            TokenType::CacheWrite => "cache_write",
        }
    }

    pub const ALL: [TokenType; 4] = [
        TokenType::Input,
        TokenType::Output,
        TokenType::CacheRead,
        TokenType::CacheWrite,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_round_trips_through_its_wire_string() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_str_opt(p.as_str()), Some(p));
            // The serde representation is the same string the rest of the
            // codebase passes around, so a JSON frame and a database column
            // can never disagree about a provider's name.
            assert_eq!(serde_json::to_value(p).unwrap(), serde_json::json!(p.as_str()));
        }
        assert_eq!(Provider::from_str_opt("nope"), None);
    }

    #[test]
    fn vendor_maps_only_to_providers_that_can_serve_it() {
        assert_eq!(
            Vendor::Anthropic.providers(),
            &[Provider::Claude, Provider::ClaudeWork]
        );
        assert_eq!(Vendor::Openai.providers(), &[Provider::Codex]);
        assert_eq!(Vendor::Google.providers(), &[Provider::Gemini]);
        assert_eq!(Vendor::from_str_opt("moonshotai"), None);
    }

    #[test]
    fn subscribable_providers_is_exactly_the_union_of_the_vendor_maps() {
        let mut union: Vec<Provider> =
            Vendor::ALL.iter().flat_map(|v| v.providers().iter().copied()).collect();
        union.sort_by_key(|p| p.as_str());
        let mut listed = SUBSCRIBABLE_PROVIDERS.to_vec();
        listed.sort_by_key(|p| p.as_str());
        assert_eq!(union, listed);
    }

    #[test]
    fn claude_work_shares_the_claude_upstream() {
        assert_eq!(Provider::ClaudeWork.upstream_family(), Provider::Claude);
        assert_eq!(Provider::Codex.upstream_family(), Provider::Codex);
    }
}
