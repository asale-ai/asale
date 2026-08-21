//! One record per upstream credential family — the file a new provider is
//! added in.
//!
//! Everything here used to be spread across both repositories as one `match`
//! arm per fact, and adding DeepSeek — a provider that needed *no new
//! behaviour*, being OpenAI-shaped, bearer-authenticated and connected by a
//! pasted key — still meant editing twenty files. Nothing about that was
//! design: the facts are tabular, so they were being written down twenty times
//! instead of once.
//!
//! Worse than the count, the copies had already drifted. The user-agent a Codex
//! request travels under was written `codex-cli` in the client and
//! `codex_cli_rs/0.146.0` in the server; only the second one was ever sent, and
//! nothing said so. `https://api.x.ai/v1` appeared in three files across two
//! repositories. This file is the single place those facts live now, and both
//! sides compile it: `asale-protocol` is a workspace member of the client and a
//! path dependency of the server, so there is one copy of these bytes, not two
//! that agree today.
//!
//! # What belongs here, and what does not
//!
//! **Data belongs here**: hosts, user-agents, labels, which wire format a
//! vendor speaks, which model ids its API answers to.
//!
//! **Behaviour does not**, and pushing it in would make this worse than the
//! duplication it replaces. Codex's entitlement discovery, Kimi's per-publisher
//! device id, Claude's OAuth system-prompt requirement, Gemini's model-in-the-
//! path URL and the three plan→cap curves are code, and they stay code. What
//! the table carries for them is the *inputs* those code paths read.
//!
//! # Which copy is authoritative
//!
//! For anything on the wire to a vendor, the **gateway** is: it builds the
//! request, and the publisher's executor only injects the credential and sends
//! it ([`ProviderSpec::chat_url`], [`ProviderSpec::user_agent`],
//! [`ProviderSpec::extra_headers`]). The client's own copy of a base URL was
//! informational — `ToolAdapter::upstream` had no caller outside its tests —
//! which is exactly how the Codex user-agents came to disagree without anything
//! failing. [`ProviderSpec::api_base`] is the one the client genuinely uses: it
//! is what a pasted key is probed against when an account is connected.

use crate::ids::{Provider, Vendor, Wire};

/// How an account of this family authenticates, and what a person needs in
/// order to connect one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// Authorization code + loopback redirect, refreshed on a timer.
    OAuth,
    /// RFC 8628 device code: approve a short code in a browser, poll for the
    /// token. What Kimi Code and the Grok CLI ship.
    DeviceFlow,
    /// A long-lived key the user pastes. `key_url` is where the vendor issues
    /// it — the connect form links straight to it, because "paste your API key"
    /// with no destination is a scavenger hunt. Empty for `custom`, whose key
    /// comes from whichever host its operator holds an account with.
    ApiKey { key_url: &'static str },
}

/// The rolling-window token allowance a lane declares against.
///
/// The estimate exists to stop a *subscription* over-declaring what its plan
/// allows. A metered key has no such window — it bills against a balance — so
/// giving it a realistic-looking number would take a busy account off the
/// market for a limit that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCap {
    /// Read from the plan name; the curve per vendor lives in
    /// `asale-client-core::discovery::plan_window_cap`.
    Plan,
    /// A flat estimate, in tokens.
    Fixed(u64),
}

/// Where this provider's real utilisation can be read, if anywhere.
///
/// Three mechanisms, not interchangeable: an endpoint can be asked while the
/// account is idle, headers can only be listened for on a response the account
/// has already served, and some providers volunteer neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSource {
    /// A dedicated endpoint that spends no quota (`oauth/usage`,
    /// `cloudcode-pa:retrieveUserQuota`, `coding/v1/usages`).
    Endpoint,
    /// Banked from the headers of responses this device serves, under the
    /// given prefix. Nothing to ask while idle.
    Headers(&'static str),
    /// Nothing readable. The Sell page says so rather than estimating.
    None,
}

/// Everything about one credential family that is a fact rather than a code
/// path.
#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub provider: Provider,
    /// The wire/database string. What travels in supply frames and task rows.
    pub id: &'static str,
    /// What a person is shown — the account list, the limits page, error text.
    pub label: &'static str,
    /// The catalog vendor whose rows this credential can serve. `None` for
    /// `custom`, which is tied to no vendor and may sell anything the platform
    /// prices.
    pub vendor: Option<Vendor>,
    pub credential: Credential,
    /// The dialect this upstream speaks. For `custom` this is only the default:
    /// its operator declares the real one per account.
    pub wire: Wire,
    /// The base a *client* addresses: what a pasted key is probed against
    /// (`{api_base}/models`). Empty where no client-side call is made.
    pub api_base: &'static str,
    /// The endpoint the *gateway* posts a relayed request to. Not derived from
    /// `api_base`: the two differ in shape per vendor (Kimi's `/coding` sits
    /// before the version, Codex has no version segment at all), and a rule
    /// that guessed would be a rule that is wrong for one of them.
    pub chat_url: &'static str,
    /// The user-agent the gateway sends. Vendors route on it.
    pub user_agent: &'static str,
    /// Headers beyond the user-agent that this upstream requires.
    pub extra_headers: &'static [(&'static str, &'static str)],
    /// Hosts a pasted key is probed against, in order. Moonshot runs two
    /// deployments and a key works on exactly one, so both are tried; every
    /// other vendor has a single host. Empty for the OAuth families, whose
    /// credential is not pasted.
    pub verify_hosts: &'static [&'static str],
    pub window_cap: WindowCap,
    pub quota: QuotaSource,
    /// LIKE-prefix that attributes a served model to this family, for local
    /// usage accounting. `None` where the family serves no fixed prefix.
    pub model_prefix: Option<&'static str>,
    /// What a fresh install advertises before its first catalog pull, so a
    /// device that starts offline still sells something. Native vendor ids —
    /// the gateway relays the requested id verbatim.
    pub fallback_models: &'static [&'static str],
    /// The ids this vendor's own API answers to, when that set is narrower
    /// than the catalog's. `None` means "advertise whatever the catalog lists".
    ///
    /// Two vendors need it, for opposite reasons. OpenRouter lists `grok-4.20`
    /// while xAI serves `grok-4.20-0309-reasoning`; DeepSeek accepts exactly
    /// two strings, each a pointer it moves to its newest re-post, while the
    /// catalog carries the dated re-posts and the whole V3/R1 back catalogue.
    /// Either way an unlisted id is matched, preauthorized, routed — and only
    /// then refused, with the publisher wearing a failure it did not cause.
    pub native_models: Option<&'static [&'static str]>,
}

impl ProviderSpec {
    /// The `key_url` of an API-key family, or `""`.
    pub const fn key_url(&self) -> &'static str {
        match self.credential {
            Credential::ApiKey { key_url } => key_url,
            _ => "",
        }
    }
}

/// Anthropic's Messages endpoint, shared by both Claude families.
const ANTHROPIC_MESSAGES: &str = "https://api.anthropic.com/v1/messages";

/// Every credential family, in the order the UI offers them.
///
/// Adding a provider is adding a row here, giving it a variant in
/// [`Provider`], and — if it is a vendor nobody sells yet — a [`Vendor`] to map
/// it to. The tests at the bottom fail if any of the three is missed.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        provider: Provider::Claude,
        id: "claude",
        label: "Claude Code",
        vendor: Some(Vendor::Anthropic),
        credential: Credential::OAuth,
        wire: Wire::Claude,
        api_base: "https://api.anthropic.com",
        chat_url: ANTHROPIC_MESSAGES,
        user_agent: "claude-cli/1.0 (external, cli)",
        extra_headers: &[("anthropic-version", "2023-06-01")],
        verify_hosts: &[],
        window_cap: WindowCap::Plan,
        quota: QuotaSource::Endpoint,
        model_prefix: Some("claude"),
        fallback_models: &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::ClaudeWork,
        id: "claude_work",
        label: "Claude Work",
        vendor: Some(Vendor::Anthropic),
        credential: Credential::OAuth,
        wire: Wire::Claude,
        api_base: "https://api.anthropic.com",
        chat_url: ANTHROPIC_MESSAGES,
        // Identical to `claude`, and that is a finding rather than a copy:
        // `ids::Provider` documents these two families as differing in exactly
        // this field, and the client's adapter did carry a distinct
        // `claude-work/1.0 (desktop)` — but the gateway builds the request that
        // is actually sent, and it has always sent the one below for both. The
        // table states what goes on the wire; making the two differ is a
        // deliberate change to upstream traffic, not a transcription.
        user_agent: "claude-cli/1.0 (external, cli)",
        extra_headers: &[("anthropic-version", "2023-06-01")],
        verify_hosts: &[],
        window_cap: WindowCap::Plan,
        quota: QuotaSource::Endpoint,
        model_prefix: Some("claude"),
        fallback_models: &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::Codex,
        id: "codex",
        // Names the account, not just the CLI: what a person connects here is
        // their ChatGPT login. The Sell page has always said so and the limits
        // page said only "Codex"; one label, and this is the one that answers
        // "which account is this?".
        label: "Codex / OpenAI",
        vendor: Some(Vendor::Openai),
        credential: Credential::OAuth,
        wire: Wire::Responses,
        api_base: "https://chatgpt.com/backend-api/codex",
        chat_url: "https://chatgpt.com/backend-api/codex/responses",
        // Versioned on purpose: `/backend-api/codex/models` answers per calling
        // version, so this has to stay in step with
        // `discovery::CODEX_CLIENT_VERSION`, which is what the model list is
        // asked for.
        user_agent: "codex_cli_rs/0.146.0",
        extra_headers: &[],
        verify_hosts: &[],
        window_cap: WindowCap::Plan,
        // No endpoint a ChatGPT bearer may read; the numbers ride back on the
        // headers of calls this device has served.
        quota: QuotaSource::Headers("x-codex-"),
        model_prefix: Some("gpt"),
        // Not `gpt-5-codex`: the ChatGPT backend a Codex subscription is served
        // by refuses that slug outright. Entitlement discovery narrows these to
        // what the account is actually granted as soon as it answers.
        fallback_models: &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4", "gpt-5.4-mini"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::Gemini,
        id: "gemini",
        label: "Gemini",
        vendor: Some(Vendor::Google),
        credential: Credential::OAuth,
        wire: Wire::Gemini,
        api_base: "https://generativelanguage.googleapis.com",
        // Gemini puts the model *and* whether it streams in the path, so the
        // rest of this URL is built per request by `translator::gemini`.
        chat_url: "https://generativelanguage.googleapis.com/v1beta/models",
        user_agent: "google-genai-cli/1.0",
        extra_headers: &[],
        verify_hosts: &[],
        window_cap: WindowCap::Plan,
        quota: QuotaSource::Endpoint,
        model_prefix: Some("gemini"),
        fallback_models: &["gemini-2.5-pro", "gemini-2.5-flash"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::Kimi,
        id: "kimi",
        label: "Kimi Code",
        vendor: Some(Vendor::Moonshotai),
        credential: Credential::DeviceFlow,
        wire: Wire::Openai,
        api_base: "https://api.kimi.com/coding",
        chat_url: "https://api.kimi.com/coding/v1/chat/completions",
        user_agent: "kimi-cli/1.0",
        // Kimi Code identifies its client through these; the per-publisher
        // device id is filled in by the executor, which is the only side that
        // knows which account is being used.
        extra_headers: &[("x-msh-platform", "kimi-cli"), ("x-msh-version", "1.0")],
        verify_hosts: &[],
        window_cap: WindowCap::Fixed(500_000),
        quota: QuotaSource::Endpoint,
        model_prefix: Some("kimi"),
        fallback_models: &["kimi-k2.7-code", "kimi-k2-thinking", "kimi-k3"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::KimiApi,
        id: "kimi_api",
        label: "Moonshot API",
        vendor: Some(Vendor::Moonshotai),
        credential: Credential::ApiKey { key_url: "https://platform.moonshot.cn/console/api-keys" },
        wire: Wire::Openai,
        api_base: "https://api.moonshot.cn/v1",
        // Two independent deployments — mainland and global — and a key issued
        // by one is rejected by the other. `ASALE_KIMI_API_BASE` selects the
        // other at the gateway; see `translator::openai::kimi_api_url`.
        chat_url: "https://api.moonshot.cn/v1/chat/completions",
        user_agent: "kimi-cli/1.0",
        extra_headers: &[],
        verify_hosts: &["https://api.moonshot.cn/v1", "https://api.moonshot.ai/v1"],
        window_cap: WindowCap::Fixed(500_000),
        quota: QuotaSource::None,
        model_prefix: Some("kimi"),
        fallback_models: &["kimi-k2.7-code", "kimi-k2-thinking", "kimi-k3"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::Xai,
        id: "xai",
        label: "Grok CLI",
        vendor: Some(Vendor::Xai),
        credential: Credential::DeviceFlow,
        wire: Wire::Responses,
        api_base: "https://cli-chat-proxy.grok.com/v1",
        chat_url: "https://cli-chat-proxy.grok.com/v1/responses",
        user_agent: "xai-grok-workspace/0.2.93",
        extra_headers: &[],
        verify_hosts: &[],
        window_cap: WindowCap::Fixed(500_000),
        // The `rest/rate-limits` call the Grok web app makes is authorised by a
        // web session, not by the CLI's bearer, so what a served response
        // volunteers is the only reading available.
        quota: QuotaSource::Headers("x-ratelimit-"),
        model_prefix: Some("grok"),
        fallback_models: &["grok-4.5", "grok-4.3", "grok-build-0.1"],
        native_models: Some(&[
            "grok-build-0.1",
            "grok-4.5",
            "grok-4.3",
            "grok-4.20-0309-reasoning",
            "grok-4.20-0309-non-reasoning",
            "grok-4.20-multi-agent-0309",
            "grok-3-mini",
            "grok-3-mini-fast",
            "grok-composer-2.5-fast",
        ]),
    },
    ProviderSpec {
        provider: Provider::XaiApi,
        id: "xai_api",
        label: "xAI API",
        vendor: Some(Vendor::Xai),
        credential: Credential::ApiKey { key_url: "https://console.x.ai" },
        wire: Wire::Openai,
        api_base: "https://api.x.ai/v1",
        chat_url: "https://api.x.ai/v1/chat/completions",
        user_agent: "grok-cli/1.0",
        extra_headers: &[],
        verify_hosts: &["https://api.x.ai/v1"],
        window_cap: WindowCap::Fixed(500_000),
        quota: QuotaSource::Headers("x-ratelimit-"),
        model_prefix: Some("grok"),
        fallback_models: &["grok-4.5", "grok-4.3", "grok-build-0.1"],
        native_models: Some(&[
            "grok-build-0.1",
            "grok-4.5",
            "grok-4.3",
            "grok-4.20-0309-reasoning",
            "grok-4.20-0309-non-reasoning",
            "grok-4.20-multi-agent-0309",
            "grok-3-mini",
            "grok-3-mini-fast",
            "grok-composer-2.5-fast",
        ]),
    },
    ProviderSpec {
        provider: Provider::Qwen,
        id: "qwen",
        label: "Alibaba Cloud Model Studio",
        vendor: Some(Vendor::Qwen),
        credential: Credential::ApiKey {
            key_url: "https://bailian.console.aliyun.com/?tab=model#/api-key",
        },
        wire: Wire::Openai,
        // The legacy Beijing endpoint remains supported and, unlike the newer
        // workspace endpoint, can be configured without a Workspace ID.
        api_base: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        chat_url: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        user_agent: "asale/1.0",
        extra_headers: &[],
        verify_hosts: &["https://dashscope.aliyuncs.com/compatible-mode/v1"],
        window_cap: WindowCap::Fixed(CUSTOM_WINDOW_TOKENS),
        quota: QuotaSource::None,
        model_prefix: Some("qwen"),
        fallback_models: &["qwen3.8-max", "qwen3.7-plus"],
        native_models: None,
    },
    ProviderSpec {
        provider: Provider::Deepseek,
        id: "deepseek",
        label: "DeepSeek",
        vendor: Some(Vendor::Deepseek),
        credential: Credential::ApiKey { key_url: "https://platform.deepseek.com/api_keys" },
        wire: Wire::Openai,
        api_base: "https://api.deepseek.com/v1",
        chat_url: "https://api.deepseek.com/v1/chat/completions",
        // No vendor CLI to impersonate — DeepSeek ships none — so the request
        // travels under asale's own name, as a custom endpoint's does.
        user_agent: "asale/1.0",
        extra_headers: &[],
        verify_hosts: &["https://api.deepseek.com/v1"],
        // Metered against a balance, like `custom`: no rolling window exists,
        // and a realistic-looking one would take a busy key off the market
        // every afternoon for a limit the vendor does not impose.
        window_cap: WindowCap::Fixed(CUSTOM_WINDOW_TOKENS),
        quota: QuotaSource::None,
        model_prefix: Some("deepseek"),
        fallback_models: &["deepseek-v4-flash", "deepseek-v4-pro"],
        // Exactly the two strings the API accepts, each a pointer the vendor
        // moves to its newest re-post ("simply use `deepseek-v4-flash` or
        // `deepseek-v4-pro` to access the latest version" —
        // <https://api-docs.deepseek.com/quick_start/models>).
        native_models: Some(&["deepseek-v4-flash", "deepseek-v4-pro"]),
    },
    ProviderSpec {
        provider: Provider::Custom,
        id: "custom",
        label: "Custom endpoint",
        // Tied to no vendor: the platform runs these itself to put supply
        // behind models its subscription sellers happen not to cover, so what
        // it may sell is everything the platform prices.
        vendor: None,
        credential: Credential::ApiKey { key_url: "" },
        // Only the default. Its operator points it at whatever host they hold a
        // key for, and the account declares the wire it speaks.
        wire: Wire::Openai,
        api_base: "",
        // A placeholder the publisher is expected to replace — the host belongs
        // to the operator. No vendor user-agent: this endpoint is not a vendor,
        // and claiming to be one of the CLIs above would be a lie some gateways
        // route on.
        chat_url: "https://custom.invalid/v1/chat/completions",
        user_agent: "asale/1.0",
        extra_headers: &[],
        verify_hosts: &[],
        window_cap: WindowCap::Fixed(CUSTOM_WINDOW_TOKENS),
        quota: QuotaSource::None,
        model_prefix: None,
        // No built-in set to fall back on — its models are whatever its
        // operator's endpoint serves — so before the first catalog pull it
        // advertises nothing rather than guessing.
        fallback_models: &[],
        native_models: None,
    },
];

/// The window declared for a metered key. Large enough never to be the binding
/// constraint, finite so the lane still declares a number the market can reason
/// about rather than an unbounded one.
pub const CUSTOM_WINDOW_TOKENS: u64 = 100_000_000;

/// This provider's record. Total: every variant has a row, which the test below
/// enforces, so a caller never has to decide what a missing one would mean.
pub fn spec(p: Provider) -> &'static ProviderSpec {
    // Linear over ten records, called on paths that then make a network
    // request — a map would cost more to build than this costs to scan.
    PROVIDERS
        .iter()
        .find(|s| s.provider == p)
        .expect("every Provider has a row in PROVIDERS (enforced by test)")
}

/// The record for a wire string, or `None` if nothing answers to it.
pub fn spec_of(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table and the enum are two halves of one thing. A variant with no
    /// row would panic in `spec` at whatever moment a user first connected it.
    #[test]
    fn every_provider_has_exactly_one_row() {
        for p in Provider::ALL {
            let rows = PROVIDERS.iter().filter(|s| s.provider == p).count();
            assert_eq!(rows, 1, "`{p}` has {rows} rows in PROVIDERS");
        }
        assert_eq!(PROVIDERS.len(), Provider::ALL.len());
    }

    /// The wire string is the row's identity: it is what a supply frame, a task
    /// row and a database column all carry.
    #[test]
    fn ids_match_the_enum_and_are_unique() {
        for s in PROVIDERS {
            assert_eq!(s.id, s.provider.as_str(), "row `{}` names a different provider", s.id);
            assert_eq!(spec_of(s.id).map(|r| r.provider), Some(s.provider));
        }
        assert!(spec_of("nope").is_none());
    }

    /// Connect method and credential shape are the same fact told twice — once
    /// as a table field, once as the predicate the connect paths branch on.
    #[test]
    fn the_credential_kind_agrees_with_the_connect_predicates() {
        for s in PROVIDERS {
            let p = s.provider;
            match s.credential {
                Credential::ApiKey { .. } => {
                    assert!(crate::ids::is_api_key_provider(p), "{p} is connected with a key");
                    assert!(!crate::ids::is_device_flow_provider(p));
                }
                Credential::DeviceFlow => {
                    assert!(crate::ids::is_device_flow_provider(p), "{p} authorises by device code");
                    assert!(!crate::ids::is_api_key_provider(p));
                }
                Credential::OAuth => {
                    assert!(!crate::ids::is_api_key_provider(p) && !crate::ids::is_device_flow_provider(p));
                }
            }
        }
    }

    /// A pasted key is probed before it is saved, so a key-connected family
    /// with nowhere to probe would accept a dead key silently. The reverse also
    /// holds: an OAuth family has no key to probe.
    #[test]
    fn a_pasted_key_has_somewhere_to_be_probed() {
        for s in PROVIDERS {
            match s.credential {
                // `custom` is the exception in both directions: its host is its
                // operator's, so there is no host written down here to probe.
                Credential::ApiKey { .. } if s.provider != Provider::Custom => {
                    assert!(!s.verify_hosts.is_empty(), "`{}` has no host to verify a key against", s.id);
                    assert!(!s.key_url().is_empty(), "`{}` gives no link to where its key is issued", s.id);
                }
                _ => assert!(s.verify_hosts.is_empty(), "`{}` has no pasted key to probe", s.id),
            }
        }
    }

    /// The vendor map and this table have to agree about which credential
    /// families can serve a catalog vendor — they are read by different sides
    /// (the client groups the catalog by provider, the server filters the board
    /// by vendor) and a disagreement makes rows invisible on one of them.
    #[test]
    fn the_vendor_map_and_the_table_agree() {
        for s in PROVIDERS {
            match s.vendor {
                Some(v) => assert!(
                    v.providers().contains(&s.provider),
                    "`{}` claims vendor `{v}`, which does not map back to it",
                    s.id
                ),
                None => assert!(
                    Vendor::ALL.iter().all(|v| !v.providers().contains(&s.provider)),
                    "`{}` claims no vendor but some vendor maps to it",
                    s.id
                ),
            }
        }
        for v in Vendor::ALL {
            for p in v.providers() {
                assert_eq!(spec(*p).vendor, Some(v), "`{v}` maps to `{p}`, which claims another vendor");
            }
        }
    }

    /// The offline fallback is advertised before anything has been pulled, so
    /// an id there that the vendor does not serve puts unusable capacity on the
    /// market for as long as the device is offline.
    #[test]
    fn the_fallback_list_only_names_ids_the_vendor_serves() {
        for s in PROVIDERS {
            let Some(native) = s.native_models else { continue };
            for m in s.fallback_models {
                assert!(native.contains(m), "`{m}` is not an id the {} API serves", s.id);
            }
        }
    }

    /// Every family that can be connected has to be able to sell something the
    /// moment it is connected, offline included.
    #[test]
    fn every_connectable_family_has_something_to_advertise() {
        for p in crate::ids::subscribable_providers() {
            assert!(!spec(p).fallback_models.is_empty(), "a fresh `{p}` install would sell nothing");
        }
    }

    /// The frontends render this table rather than retyping it, so the file
    /// they render has to still match the table it was rendered from.
    #[test]
    fn the_generated_typescript_is_up_to_date() {
        // This repository owns the desktop client's copy; the gateway's test
        // owns the web console's.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../shared/providers.generated.ts");
        check_generated_typescript(&path);
    }

    /// The gateway posts to `chat_url` and a client probes `api_base`; both are
    /// vendor hosts, and an empty one would send a request to a relative path.
    #[test]
    fn every_vendor_family_names_a_real_host() {
        for s in PROVIDERS.iter().filter(|s| s.provider != Provider::Custom) {
            assert!(s.chat_url.starts_with("https://"), "`{}` has no upstream", s.id);
            assert!(s.api_base.starts_with("https://"), "`{}` has no base to probe", s.id);
            assert!(!s.user_agent.is_empty(), "`{}` travels with no user-agent", s.id);
            for host in s.verify_hosts {
                assert!(host.starts_with("https://"), "`{}` probes a non-https host", s.id);
            }
        }
    }
}

// ── The TypeScript view of this table ───────────────────────────────

/// Render the slice of this table the two frontends need, as a TypeScript
/// module.
///
/// Generated rather than shared, because a module cannot be: `@shared/*` is a
/// tsconfig path into the desktop client's tree and carries **types only** —
/// they are erased before bundling, while runtime data is not. The web console
/// is a separate Next project that cannot resolve a module above its own root,
/// and the desktop client ships as its own repository that must not depend on
/// files in the other. That constraint is why `api-compat.ts` exists twice and
/// is kept in step by hand; this is the same constraint answered by generating
/// the copies from one source instead of retyping them.
///
/// Each side owns the copy that lives in it, and each side's test fails when it
/// drifts (`UPDATE_GENERATED=1 cargo test` rewrites it). Only what a UI
/// genuinely renders is included — hosts, user-agents and model lists are the
/// gateway's business and have no place in a bundle.
pub fn render_typescript() -> String {
    let mut out = String::new();
    out.push_str(
        "// @generated by `asale_protocol::providers::render_typescript` — do not edit.\n\
         //\n\
         // Regenerate with `UPDATE_GENERATED=1 cargo test` in whichever repository\n\
         // owns this copy; the source is `asale-client/protocol/src/providers.rs`,\n\
         // which both the desktop client and the gateway compile.\n\
         \n\
         /** How an account of this family is connected. */\n\
         export type ProviderCredential = \"oauth\" | \"device_flow\" | \"api_key\";\n\
         \n\
         export interface ProviderInfo {\n\
         \x20 /** Wire id — what the daemon and the server call this family. */\n\
         \x20 id: string;\n\
         \x20 /** What a person is shown. */\n\
         \x20 label: string;\n\
         \x20 credential: ProviderCredential;\n\
         \x20 /** Where the vendor issues the key, for the key-connected families. */\n\
         \x20 keyUrl: string;\n\
         \x20 /** Catalog vendor slug this credential serves, `\"\"` for none. */\n\
         \x20 vendor: string;\n\
         \x20 /** False for families the platform runs itself and never offers. */\n\
         \x20 connectable: boolean;\n\
         }\n\
         \n\
         export const PROVIDERS: ProviderInfo[] = [\n",
    );
    for s in PROVIDERS {
        let credential = match s.credential {
            Credential::OAuth => "oauth",
            Credential::DeviceFlow => "device_flow",
            Credential::ApiKey { .. } => "api_key",
        };
        let connectable = s.vendor.is_some();
        out.push_str(&format!(
            "  {{ id: {:?}, label: {:?}, credential: {:?}, keyUrl: {:?}, vendor: {:?}, connectable: {} }},\n",
            s.id,
            s.label,
            credential,
            s.key_url(),
            s.vendor.map(|v| v.as_str()).unwrap_or(""),
            connectable,
        ));
    }
    out.push_str("];\n\n/** Catalog vendor slug → brand casing. */\nexport const VENDOR_LABELS: Record<string, string> = {\n");
    for v in crate::ids::Vendor::ALL {
        out.push_str(&format!("  {:?}: {:?},\n", v.as_str(), v.label()));
    }
    out.push_str("};\n");
    out
}

/// Compare a checked-in generated file against [`render_typescript`], rewriting
/// it when `UPDATE_GENERATED=1`.
///
/// Shared by both repositories' tests so the staleness check reads the same on
/// each side.
pub fn check_generated_typescript(path: &std::path::Path) {
    let want = render_typescript();
    let have = std::fs::read_to_string(path).unwrap_or_default();
    if have == want {
        return;
    }
    if std::env::var("UPDATE_GENERATED").is_ok_and(|v| v == "1") {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create generated dir");
        }
        std::fs::write(path, &want).expect("write generated file");
        return;
    }
    panic!(
        "`{}` is out of date with `providers::PROVIDERS`.\n\
         Regenerate it: UPDATE_GENERATED=1 cargo test",
        path.display()
    );
}
