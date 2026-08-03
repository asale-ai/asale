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
//!   `anthropic`, `openai`, `google`, `moonshotai`, `x-ai`.
//! * a *tool* — a locally installed AI CLI whose config the buy switch rewrites
//!   (Claude Code, Codex, Gemini CLI). Purely client-side and never on the
//!   wire, so it lives in the daemon (`tool_config::Tool`), not here.

use serde::{Deserialize, Serialize};

/// Upstream token family a publisher injects credentials for.
///
/// `claude_work` shares the claude upstream URL and translator; only the
/// user-agent differs.
///
/// Moonshot and xAI each appear twice, because with them a vendor is not one
/// endpoint. The coding **subscription** and the metered **platform API key**
/// are separate products on separate hosts speaking different wire formats, and
/// a credential for one is rejected by the other — so they cannot share a
/// provider id the way `claude`/`claude_work` do:
///
/// | id         | credential           | upstream                              |
/// |------------|----------------------|---------------------------------------|
/// | `kimi`     | Kimi Code OAuth      | `api.kimi.com/coding` (chat)          |
/// | `kimi_api` | Moonshot API key     | `api.moonshot.cn` (chat)              |
/// | `xai`      | Grok CLI OAuth       | `cli-chat-proxy.grok.com` (responses) |
/// | `xai_api`  | xAI API key          | `api.x.ai` (chat)                     |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    ClaudeWork,
    Codex,
    Gemini,
    Kimi,
    KimiApi,
    Xai,
    XaiApi,
    /// An OpenAI-compatible endpoint reached with a pasted key and a base URL
    /// the operator supplies — the one provider whose upstream is not known at
    /// compile time.
    ///
    /// Internal: no vendor maps to it (so it is absent from
    /// [`SUBSCRIBABLE_PROVIDERS`] and never offered as a connectable account),
    /// and the platform runs these itself to put supply behind models its
    /// subscription sellers happen not to cover. Everything downstream —
    /// matching, metering, settlement, reputation — treats it as an ordinary
    /// publisher, because that is exactly what it is.
    Custom,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::ClaudeWork => "claude_work",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
            Provider::Kimi => "kimi",
            Provider::KimiApi => "kimi_api",
            Provider::Xai => "xai",
            Provider::XaiApi => "xai_api",
            Provider::Custom => "custom",
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
            "kimi_api" => Provider::KimiApi,
            "xai" => Provider::Xai,
            "xai_api" => Provider::XaiApi,
            "custom" => Provider::Custom,
            _ => return None,
        })
    }

    /// The upstream family, which drives translator + URL choice. `claude_work`
    /// maps to the claude family (same upstream, different UA).
    ///
    /// The Kimi and xAI pairs deliberately do *not* collapse: their two ids
    /// exist precisely because the upstream differs.
    pub fn upstream_family(&self) -> Provider {
        match self {
            Provider::ClaudeWork => Provider::Claude,
            other => *other,
        }
    }

    /// The label a person recognises, for UI and error messages.
    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::ClaudeWork => "Claude Work",
            Provider::Codex => "Codex / OpenAI",
            Provider::Gemini => "Gemini",
            Provider::Kimi => "Kimi Code",
            Provider::KimiApi => "Moonshot API",
            Provider::Xai => "Grok CLI",
            Provider::XaiApi => "xAI API",
            Provider::Custom => "Custom endpoint",
        }
    }

    pub const ALL: [Provider; 9] = [
        Provider::Claude,
        Provider::ClaudeWork,
        Provider::Codex,
        Provider::Gemini,
        Provider::Kimi,
        Provider::KimiApi,
        Provider::Xai,
        Provider::XaiApi,
        Provider::Custom,
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
    Moonshotai,
    /// OpenRouter spells this one with a hyphen (`x-ai/grok-4.5`), which no
    /// `rename_all` rule produces — hence the explicit rename. The slug is the
    /// contract with `prices.provider`, so getting it wrong silently maps every
    /// Grok row to no provider at all.
    #[serde(rename = "x-ai")]
    Xai,
}

impl Vendor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::Openai => "openai",
            Vendor::Google => "google",
            Vendor::Moonshotai => "moonshotai",
            Vendor::Xai => "x-ai",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Vendor> {
        Some(match s {
            "anthropic" => Vendor::Anthropic,
            "openai" => Vendor::Openai,
            "google" => Vendor::Google,
            "moonshotai" => Vendor::Moonshotai,
            "x-ai" => Vendor::Xai,
            _ => return None,
        })
    }

    /// The credential families whose subscriptions can serve this vendor's
    /// models.
    pub fn providers(&self) -> &'static [Provider] {
        match self {
            Vendor::Anthropic => &[Provider::Claude, Provider::ClaudeWork],
            Vendor::Openai => &[Provider::Codex],
            Vendor::Google => &[Provider::Gemini],
            Vendor::Moonshotai => &[Provider::Kimi, Provider::KimiApi],
            Vendor::Xai => &[Provider::Xai, Provider::XaiApi],
        }
    }

    pub const ALL: [Vendor; 5] =
        [Vendor::Anthropic, Vendor::Openai, Vendor::Google, Vendor::Moonshotai, Vendor::Xai];
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
    Provider::Kimi,
    Provider::KimiApi,
    Provider::Xai,
    Provider::XaiApi,
];

/// Whether a provider authenticates with a long-lived API key rather than an
/// OAuth token that expires and is refreshed.
///
/// These accounts are connected by pasting the key, carry no expiry, and are
/// skipped by the refresh loop. Everything downstream (injection, metering, the
/// sell switch) is identical to an OAuth account.
pub fn is_api_key_provider(p: Provider) -> bool {
    matches!(p, Provider::KimiApi | Provider::XaiApi | Provider::Custom)
}

/// Whether connecting this provider runs an RFC 8628 device-code flow rather
/// than the authorization-code + loopback flow the others use.
///
/// Moonshot and xAI both ship their coding subscription through a CLI that
/// authorises by device code: there is no redirect URI to register, the user
/// approves a short code in a browser, and the daemon polls for the token.
pub fn is_device_flow_provider(p: Provider) -> bool {
    matches!(p, Provider::Kimi | Provider::Xai)
}

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
    #[test]
    fn custom_is_sellable_but_never_offered_as_a_subscription() {
        use super::*;
        // It round-trips like any other provider — supply frames, task rows and
        // metering all carry it as a string.
        assert_eq!(Provider::from_str_opt("custom"), Some(Provider::Custom));
        assert_eq!(Provider::Custom.as_str(), "custom");
        // Its credential is a pasted key, so the refresh loop leaves it alone.
        assert!(is_api_key_provider(Provider::Custom));
        assert!(!is_device_flow_provider(Provider::Custom));
        // No vendor maps to it: it is internal, and the account list the UI
        // offers to connect is built from the vendor map.
        assert!(!SUBSCRIBABLE_PROVIDERS.contains(&Provider::Custom));
        assert!(Vendor::ALL.iter().all(|v| !v.providers().contains(&Provider::Custom)));
        // Its upstream is its own — collapsing it into another family would
        // send an operator's key to that vendor's host.
        assert_eq!(Provider::Custom.upstream_family(), Provider::Custom);
    }

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
        // Both credential kinds serve the same catalog rows; they differ in
        // which host the gateway sends the request to, not in what they can do.
        assert_eq!(Vendor::Moonshotai.providers(), &[Provider::Kimi, Provider::KimiApi]);
        assert_eq!(Vendor::Xai.providers(), &[Provider::Xai, Provider::XaiApi]);
        assert_eq!(Vendor::from_str_opt("nope"), None);
    }

    #[test]
    fn a_subscription_and_a_platform_key_are_never_the_same_provider() {
        // They reach different hosts, so collapsing them would send a Kimi Code
        // OAuth token to the Moonshot platform API (or the reverse) and 401.
        assert_ne!(Provider::Kimi.upstream_family(), Provider::KimiApi.upstream_family());
        assert_ne!(Provider::Xai.upstream_family(), Provider::XaiApi.upstream_family());
        for p in [Provider::Kimi, Provider::KimiApi, Provider::Xai, Provider::XaiApi] {
            assert_eq!(p.upstream_family(), p, "{p} must keep its own upstream");
        }
        // Exactly one of the two flavours per vendor is a pasted key.
        assert!(is_api_key_provider(Provider::KimiApi) && !is_api_key_provider(Provider::Kimi));
        assert!(is_api_key_provider(Provider::XaiApi) && !is_api_key_provider(Provider::Xai));
        assert!(is_device_flow_provider(Provider::Kimi) && is_device_flow_provider(Provider::Xai));
        for p in [Provider::Claude, Provider::Codex, Provider::Gemini] {
            assert!(!is_device_flow_provider(p) && !is_api_key_provider(p));
        }
    }

    #[test]
    fn vendor_round_trips_through_its_catalog_slug() {
        // `prices.provider` holds these strings verbatim, and the hyphen in
        // `x-ai` is exactly the kind of thing a derive would quietly get wrong.
        for v in Vendor::ALL {
            assert_eq!(Vendor::from_str_opt(v.as_str()), Some(v));
            assert_eq!(serde_json::to_value(v).unwrap(), serde_json::json!(v.as_str()));
        }
        assert_eq!(Vendor::Xai.as_str(), "x-ai");
    }

    #[test]
    fn every_provider_has_exactly_one_connect_method() {
        // A provider that is neither a device flow nor a pasted key connects
        // through the authorization-code flow; being classified as two at once
        // would make the Sell page offer two buttons for one account type.
        for p in Provider::ALL {
            assert!(
                !(is_api_key_provider(p) && is_device_flow_provider(p)),
                "{p} claims two connect methods"
            );
        }
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
