//! The identifier vocabulary both sides share.
//!
//! Three different things in this system were all called "provider", which is
//! why the same list of strings kept being written out by hand in a different
//! shape in each module:
//!
//! * [`Provider`] — an **upstream credential family**. What a publisher holds a
//!   subscription for and injects a token for: `claude`, `claude_work`,
//!   `codex`, `gemini`, `kimi`, `xai`, `deepseek`. This is what travels on
//!   the wire.
//! * [`Vendor`] — a **catalog vendor slug**, the left half of an OpenRouter
//!   model id (`anthropic/claude-opus-4`) and the `prices.provider` column:
//!   `anthropic`, `openai`, `google`, `moonshotai`, `deepseek`, `x-ai`.
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
///
/// DeepSeek needs no such pair: it ships no coding subscription, only the
/// metered platform key, so the one flavour keeps the plain vendor name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    ClaudeWork,
    /// A Claude subscription whose owner has turned on pay-as-you-go **extra
    /// usage**, declared as a lane of its own rather than folded into
    /// [`Provider::Claude`].
    ///
    /// Same account, same OAuth client, same upstream — and deliberately a
    /// separate credential family, because what a buyer may be served from is
    /// not the same for the two. Anthropic's terms confine a plain subscription
    /// bearer to its own products; the overflow an extra-usage account serves is
    /// metered and billed per token, which is the footing every third-party tool
    /// is entitled to be on. See [`crate::providers::denied_providers`].
    ///
    /// Splitting it also keeps the window honest: the subscription lane sells
    /// against a plan window that runs out, and this one bills against a
    /// balance, so folding them together would take a paying account off the
    /// market for a limit it is no longer under.
    ClaudeExtra,
    Codex,
    Gemini,
    Kimi,
    KimiApi,
    Xai,
    XaiApi,
    /// Alibaba Cloud Model Studio (DashScope) API key.
    Qwen,
    /// DeepSeek's platform API key. One flavour only — see the table above.
    Deepseek,
    /// An OpenRouter API key.
    ///
    /// The one credential family that is an *aggregator* rather than a vendor:
    /// one key reaches every model OpenRouter routes, in every modality it
    /// routes them in — text, and the image, video, speech and transcription
    /// endpoints no subscription produces. That is why it is the family the
    /// non-text market is built on, and why it resells other vendors' rows
    /// ([`crate::providers::resells_other_vendors`]).
    ///
    /// Not offered by a stock connect screen: like every metered key, whether
    /// an account may connect one is the server's answer rather than a flag
    /// compiled in here. See [`crate::providers::ProviderSpec::offered_by_default`].
    Openrouter,
    /// An OpenAI-compatible endpoint reached with a pasted key and a base URL
    /// the operator supplies — the one provider whose upstream is not known at
    /// compile time.
    ///
    /// No vendor maps to it, so it is absent from `subscribable_providers` and
    /// is not drawn by a stock connect screen — whether an account may connect
    /// one is the server's answer, like every other metered key. Everything
    /// downstream — matching, metering, settlement, reputation — treats it as
    /// an ordinary publisher, because that is exactly what it is.
    Custom,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        crate::providers::spec(*self).id
    }

    /// Parse a wire/database string. `None` for anything unknown — callers
    /// decide whether that is a rejection or a skip.
    pub fn from_str_opt(s: &str) -> Option<Provider> {
        crate::providers::spec_of(s).map(|r| r.provider)
    }

    /// The upstream family, which drives translator + URL choice. `claude_work`
    /// maps to the claude family (same upstream, different UA).
    ///
    /// The Kimi and xAI pairs deliberately do *not* collapse: their two ids
    /// exist precisely because the upstream differs.
    pub fn upstream_family(&self) -> Provider {
        match self {
            Provider::ClaudeWork | Provider::ClaudeExtra => Provider::Claude,
            other => *other,
        }
    }

    /// The label a person recognises, for UI and error messages.
    pub fn display_name(&self) -> &'static str {
        crate::providers::spec(*self).label
    }

    /// Does this family reach Anthropic's Messages endpoint with a Claude
    /// OAuth bearer?
    ///
    /// Three do now, and the count is the reason this exists: the client used to
    /// spell it `"claude" | "claude_work"` in a dozen `match` arms, so adding a
    /// third meant finding all twelve and the one that was missed would be a
    /// silent wrong answer rather than a compile error.
    pub fn is_claude_family(&self) -> bool {
        self.upstream_family() == Provider::Claude
    }

    pub const ALL: [Provider; 13] = [
        Provider::Claude,
        Provider::ClaudeWork,
        Provider::ClaudeExtra,
        Provider::Codex,
        Provider::Gemini,
        Provider::Kimi,
        Provider::KimiApi,
        Provider::Xai,
        Provider::XaiApi,
        Provider::Qwen,
        Provider::Deepseek,
        Provider::Openrouter,
        Provider::Custom,
    ];
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The wire format an upstream speaks.
///
/// Every vendor's own API has exactly one, and which one is settled at compile
/// time by the [`Provider`]: `claude` answers Anthropic Messages, `codex`
/// answers OpenAI Responses. A custom endpoint is the exception — its operator
/// points it at whatever host they hold a key for — so there the wire is a
/// property of the *account*, declared with its supply and carried to the
/// gateway, which is what decides the body it builds for that lane.
///
/// The four are exactly the dialects the gateway can translate between; a lane
/// speaking anything else is one the market has no way to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// OpenAI chat completions — `POST {base}/chat/completions`, bearer key.
    Openai,
    /// OpenAI Responses — `POST {base}/responses`, bearer key.
    Responses,
    /// Anthropic Messages — `POST {base}/messages`, `x-api-key` and a version
    /// header rather than a bearer.
    Claude,
    /// Google Generative Language — `POST {base}/models/{model}:generateContent`,
    /// which puts the model *and* whether it streams in the path, keyed by
    /// `x-goog-api-key`.
    Gemini,
}

impl Wire {
    pub fn as_str(&self) -> &'static str {
        match self {
            Wire::Openai => "openai",
            Wire::Responses => "responses",
            Wire::Claude => "claude",
            Wire::Gemini => "gemini",
        }
    }

    /// Parse a wire/settings string. `None` for anything unknown — including
    /// the empty string a publisher sends for a lane whose provider settles its
    /// own wire, which is why callers treat `None` as "ask the provider" rather
    /// than as an error.
    pub fn from_str_opt(s: &str) -> Option<Wire> {
        Some(match s.trim() {
            "openai" => Wire::Openai,
            "responses" => Wire::Responses,
            "claude" => Wire::Claude,
            "gemini" => Wire::Gemini,
            _ => return None,
        })
    }

    /// What the connect form offers, in the order it offers them.
    pub const ALL: [Wire; 4] = [Wire::Openai, Wire::Claude, Wire::Gemini, Wire::Responses];
}

impl Default for Wire {
    /// What a custom endpoint is assumed to speak when nobody said: the schema
    /// the great majority of resellable endpoints serve, and the one every such
    /// account spoke before the wire was a choice at all.
    fn default() -> Wire {
        Wire::Openai
    }
}

impl std::fmt::Display for Wire {
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
    Qwen,
    Deepseek,
    /// OpenRouter spells this one with a hyphen (`x-ai/grok-4.5`), which no
    /// `rename_all` rule produces — hence the explicit rename. The slug is the
    /// contract with `prices.provider`, so getting it wrong silently maps every
    /// Grok row to no provider at all.
    #[serde(rename = "x-ai")]
    Xai,
    /// Zhipu AI's GLM family. The catalog files it under the company's
    /// international brand, `z-ai`, which is also the id its own API uses.
    #[serde(rename = "z-ai")]
    Zhipu,
    Minimax,
    /// Xiaomi's MiMo. No platform of its own — the models are served through
    /// Model Studio, which is the whole reason [`Vendor::providers`] maps a
    /// vendor to a reseller here.
    Xiaomi,
    /// OpenRouter's own rows (`openrouter/auto` and friends), and the vendor a
    /// key of theirs belongs to. Every *other* vendor also maps to
    /// [`Provider::Openrouter`], because one key serves all of them.
    Openrouter,
    // ── Media vendors ───────────────────────────────────────────────
    //
    // Everything below ships image, video, speech or transcription models and
    // nothing else this platform trades. None of them has a credential family
    // here — there is no Recraft subscription to import — so all of them map to
    // [`Provider::Openrouter`] alone, which is the only family with an endpoint
    // for what they make.
    //
    // They are listed for one reason: a vendor absent from this enum is a
    // vendor whose rows `api::market::is_sellable_vendor` drops from the board,
    // so its models are priced, sellable and invisible.
    Alibaba,
    #[serde(rename = "black-forest-labs")]
    BlackForestLabs,
    Bytedance,
    #[serde(rename = "bytedance-seed")]
    BytedanceSeed,
    Canopylabs,
    Deepgram,
    #[serde(rename = "fish-audio")]
    FishAudio,
    Heygen,
    Hexgrad,
    Krea,
    /// Kuaishou's video group, which publishes the Kling models under this slug.
    Kwaivgi,
    Meta,
    Microsoft,
    Mistralai,
    Nvidia,
    Recraft,
    Runway,
    Sesame,
    Sourceful,
    // ── Text vendors the aggregator is the only seller for ─────────
    //
    // Same rule as the media vendors above, one modality over: these ship chat
    // models, nobody sells a subscription to any of them, and a seller with an
    // OpenRouter key is the only supply their rows can ever have. Absent from
    // this enum they were priced and enabled and reached nobody — dropped from
    // the board by `api::market::is_sellable_vendor`, and dropped again by the
    // client's `publisher::providers_for_vendor`, which maps an unknown slug to
    // no credential family and so never advertises it.
    Ai21,
    #[serde(rename = "aion-labs")]
    AionLabs,
    Allenai,
    Amazon,
    #[serde(rename = "anthracite-org")]
    AnthraciteOrg,
    #[serde(rename = "arcee-ai")]
    ArceeAi,
    Baidu,
    Cognitivecomputations,
    Cohere,
    Deepcogito,
    Gryphe,
    #[serde(rename = "ibm-granite")]
    IbmGranite,
    Inception,
    Inclusionai,
    Inflection,
    Kwaipilot,
    Mancer,
    Meituan,
    #[serde(rename = "meta-llama")]
    MetaLlama,
    Morph,
    #[serde(rename = "nex-agi")]
    NexAgi,
    Nousresearch,
    Perceptron,
    Perplexity,
    Poolside,
    Rekaai,
    Relace,
    Sakana,
    Sao10k,
    Stepfun,
    Tencent,
    Thedrummer,
    Thinkingmachines,
    Undi95,
    Upstage,
    Writer,
}

impl Vendor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::Openai => "openai",
            Vendor::Google => "google",
            Vendor::Moonshotai => "moonshotai",
            Vendor::Qwen => "qwen",
            Vendor::Deepseek => "deepseek",
            Vendor::Xai => "x-ai",
            Vendor::Zhipu => "z-ai",
            Vendor::Minimax => "minimax",
            Vendor::Xiaomi => "xiaomi",
            Vendor::Openrouter => "openrouter",
            Vendor::Alibaba => "alibaba",
            Vendor::BlackForestLabs => "black-forest-labs",
            Vendor::Bytedance => "bytedance",
            Vendor::BytedanceSeed => "bytedance-seed",
            Vendor::Canopylabs => "canopylabs",
            Vendor::Deepgram => "deepgram",
            Vendor::FishAudio => "fish-audio",
            Vendor::Heygen => "heygen",
            Vendor::Hexgrad => "hexgrad",
            Vendor::Krea => "krea",
            Vendor::Kwaivgi => "kwaivgi",
            Vendor::Meta => "meta",
            Vendor::Microsoft => "microsoft",
            Vendor::Mistralai => "mistralai",
            Vendor::Nvidia => "nvidia",
            Vendor::Recraft => "recraft",
            Vendor::Runway => "runway",
            Vendor::Sesame => "sesame",
            Vendor::Sourceful => "sourceful",
            Vendor::Ai21 => "ai21",
            Vendor::AionLabs => "aion-labs",
            Vendor::Allenai => "allenai",
            Vendor::Amazon => "amazon",
            Vendor::AnthraciteOrg => "anthracite-org",
            Vendor::ArceeAi => "arcee-ai",
            Vendor::Baidu => "baidu",
            Vendor::Cognitivecomputations => "cognitivecomputations",
            Vendor::Cohere => "cohere",
            Vendor::Deepcogito => "deepcogito",
            Vendor::Gryphe => "gryphe",
            Vendor::IbmGranite => "ibm-granite",
            Vendor::Inception => "inception",
            Vendor::Inclusionai => "inclusionai",
            Vendor::Inflection => "inflection",
            Vendor::Kwaipilot => "kwaipilot",
            Vendor::Mancer => "mancer",
            Vendor::Meituan => "meituan",
            Vendor::MetaLlama => "meta-llama",
            Vendor::Morph => "morph",
            Vendor::NexAgi => "nex-agi",
            Vendor::Nousresearch => "nousresearch",
            Vendor::Perceptron => "perceptron",
            Vendor::Perplexity => "perplexity",
            Vendor::Poolside => "poolside",
            Vendor::Rekaai => "rekaai",
            Vendor::Relace => "relace",
            Vendor::Sakana => "sakana",
            Vendor::Sao10k => "sao10k",
            Vendor::Stepfun => "stepfun",
            Vendor::Tencent => "tencent",
            Vendor::Thedrummer => "thedrummer",
            Vendor::Thinkingmachines => "thinkingmachines",
            Vendor::Undi95 => "undi95",
            Vendor::Upstage => "upstage",
            Vendor::Writer => "writer",
        }
    }

    /// Brand casing for a vendor's slug — what a board rail or a legend shows.
    ///
    /// The slug is lowercase and hyphenated because it is a catalog key
    /// (`x-ai/grok-4.5`); printing it raw spells two of the six wrong.
    pub fn label(&self) -> &'static str {
        match self {
            Vendor::Anthropic => "Anthropic",
            Vendor::Openai => "OpenAI",
            Vendor::Google => "Google",
            Vendor::Moonshotai => "Moonshot",
            Vendor::Qwen => "Qwen",
            Vendor::Deepseek => "DeepSeek",
            Vendor::Xai => "xAI",
            Vendor::Zhipu => "Z.ai",
            Vendor::Minimax => "MiniMax",
            Vendor::Xiaomi => "Xiaomi",
            Vendor::Openrouter => "OpenRouter",
            Vendor::Alibaba => "Alibaba",
            Vendor::BlackForestLabs => "Black Forest Labs",
            Vendor::Bytedance => "ByteDance",
            Vendor::BytedanceSeed => "ByteDance Seed",
            Vendor::Canopylabs => "Canopy Labs",
            Vendor::Deepgram => "Deepgram",
            Vendor::FishAudio => "Fish Audio",
            Vendor::Heygen => "HeyGen",
            Vendor::Hexgrad => "Hexgrad",
            Vendor::Krea => "Krea",
            Vendor::Kwaivgi => "KwaiVGI",
            Vendor::Meta => "Meta",
            Vendor::Microsoft => "Microsoft",
            Vendor::Mistralai => "Mistral",
            Vendor::Nvidia => "NVIDIA",
            Vendor::Recraft => "Recraft",
            Vendor::Runway => "Runway",
            Vendor::Sesame => "Sesame",
            Vendor::Sourceful => "Sourceful",
            Vendor::Ai21 => "AI21",
            Vendor::AionLabs => "AionLabs",
            Vendor::Allenai => "AllenAI",
            Vendor::Amazon => "Amazon",
            Vendor::AnthraciteOrg => "Anthracite",
            Vendor::ArceeAi => "Arcee AI",
            Vendor::Baidu => "Baidu",
            Vendor::Cognitivecomputations => "Cognitive Computations",
            Vendor::Cohere => "Cohere",
            Vendor::Deepcogito => "Deep Cogito",
            Vendor::Gryphe => "Gryphe",
            Vendor::IbmGranite => "IBM Granite",
            Vendor::Inception => "Inception",
            Vendor::Inclusionai => "InclusionAI",
            Vendor::Inflection => "Inflection",
            Vendor::Kwaipilot => "KwaiPilot",
            Vendor::Mancer => "Mancer",
            Vendor::Meituan => "Meituan",
            Vendor::MetaLlama => "Meta Llama",
            Vendor::Morph => "Morph",
            Vendor::NexAgi => "NEX AGI",
            Vendor::Nousresearch => "Nous Research",
            Vendor::Perceptron => "Perceptron",
            Vendor::Perplexity => "Perplexity",
            Vendor::Poolside => "Poolside",
            Vendor::Rekaai => "Reka",
            Vendor::Relace => "Relace",
            Vendor::Sakana => "Sakana AI",
            Vendor::Sao10k => "Sao10K",
            Vendor::Stepfun => "StepFun",
            Vendor::Tencent => "Tencent",
            Vendor::Thedrummer => "TheDrummer",
            Vendor::Thinkingmachines => "Thinking Machines",
            Vendor::Undi95 => "Undi95",
            Vendor::Upstage => "Upstage",
            Vendor::Writer => "Writer",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Vendor> {
        Some(match s {
            "anthropic" => Vendor::Anthropic,
            "openai" => Vendor::Openai,
            "google" => Vendor::Google,
            "moonshotai" => Vendor::Moonshotai,
            "qwen" => Vendor::Qwen,
            "deepseek" => Vendor::Deepseek,
            "x-ai" => Vendor::Xai,
            "z-ai" => Vendor::Zhipu,
            "minimax" => Vendor::Minimax,
            "xiaomi" => Vendor::Xiaomi,
            "openrouter" => Vendor::Openrouter,
            "alibaba" => Vendor::Alibaba,
            "black-forest-labs" => Vendor::BlackForestLabs,
            "bytedance" => Vendor::Bytedance,
            "bytedance-seed" => Vendor::BytedanceSeed,
            "canopylabs" => Vendor::Canopylabs,
            "deepgram" => Vendor::Deepgram,
            "fish-audio" => Vendor::FishAudio,
            "heygen" => Vendor::Heygen,
            "hexgrad" => Vendor::Hexgrad,
            "krea" => Vendor::Krea,
            "kwaivgi" => Vendor::Kwaivgi,
            "meta" => Vendor::Meta,
            "microsoft" => Vendor::Microsoft,
            "mistralai" => Vendor::Mistralai,
            "nvidia" => Vendor::Nvidia,
            "recraft" => Vendor::Recraft,
            "runway" => Vendor::Runway,
            "sesame" => Vendor::Sesame,
            "sourceful" => Vendor::Sourceful,
            "ai21" => Vendor::Ai21,
            "aion-labs" => Vendor::AionLabs,
            "allenai" => Vendor::Allenai,
            "amazon" => Vendor::Amazon,
            "anthracite-org" => Vendor::AnthraciteOrg,
            "arcee-ai" => Vendor::ArceeAi,
            "baidu" => Vendor::Baidu,
            "cognitivecomputations" => Vendor::Cognitivecomputations,
            "cohere" => Vendor::Cohere,
            "deepcogito" => Vendor::Deepcogito,
            "gryphe" => Vendor::Gryphe,
            "ibm-granite" => Vendor::IbmGranite,
            "inception" => Vendor::Inception,
            "inclusionai" => Vendor::Inclusionai,
            "inflection" => Vendor::Inflection,
            "kwaipilot" => Vendor::Kwaipilot,
            "mancer" => Vendor::Mancer,
            "meituan" => Vendor::Meituan,
            "meta-llama" => Vendor::MetaLlama,
            "morph" => Vendor::Morph,
            "nex-agi" => Vendor::NexAgi,
            "nousresearch" => Vendor::Nousresearch,
            "perceptron" => Vendor::Perceptron,
            "perplexity" => Vendor::Perplexity,
            "poolside" => Vendor::Poolside,
            "rekaai" => Vendor::Rekaai,
            "relace" => Vendor::Relace,
            "sakana" => Vendor::Sakana,
            "sao10k" => Vendor::Sao10k,
            "stepfun" => Vendor::Stepfun,
            "tencent" => Vendor::Tencent,
            "thedrummer" => Vendor::Thedrummer,
            "thinkingmachines" => Vendor::Thinkingmachines,
            "undi95" => Vendor::Undi95,
            "upstage" => Vendor::Upstage,
            "writer" => Vendor::Writer,
            _ => return None,
        })
    }

    /// The credential families whose subscriptions can serve this vendor's
    /// models.
    ///
    /// Written down rather than derived from `providers::PROVIDERS`, because
    /// what it carries beyond the mapping is the *order* a vendor's two
    /// flavours are offered in. `providers::tests::the_vendor_map_and_the_table
    /// _agree` holds the two halves in step.
    pub fn providers(&self) -> &'static [Provider] {
        match self {
            Vendor::Anthropic => &[
                Provider::Claude,
                Provider::ClaudeWork,
                Provider::ClaudeExtra,
                Provider::Openrouter,
            ],
            Vendor::Openai => &[Provider::Codex, Provider::Openrouter],
            Vendor::Google => &[Provider::Gemini, Provider::Openrouter],
            Vendor::Moonshotai => &[Provider::Kimi, Provider::KimiApi, Provider::Openrouter],
            Vendor::Qwen => &[Provider::Qwen, Provider::Openrouter],
            Vendor::Deepseek => &[Provider::Deepseek, Provider::Openrouter],
            Vendor::Xai => &[Provider::Xai, Provider::XaiApi, Provider::Openrouter],
            // These three ship no credential family of their own on this
            // platform: Alibaba's Model Studio resells them, so a Model Studio
            // key is what serves their rows. `providers::resells_other_vendors`
            // is what makes that mapping legal — see the test there.
            Vendor::Zhipu | Vendor::Minimax | Vendor::Xiaomi => {
                &[Provider::Qwen, Provider::Openrouter]
            }
            // Nobody sells a Recraft or Deepgram subscription; the
            // aggregator is the only family with an endpoint for what these
            // vendors make.
            Vendor::Openrouter
            | Vendor::Alibaba
            | Vendor::BlackForestLabs
            | Vendor::Bytedance
            | Vendor::BytedanceSeed
            | Vendor::Canopylabs
            | Vendor::Deepgram
            | Vendor::FishAudio
            | Vendor::Heygen
            | Vendor::Hexgrad
            | Vendor::Krea
            | Vendor::Kwaivgi
            | Vendor::Meta
            | Vendor::Microsoft
            | Vendor::Mistralai
            | Vendor::Nvidia
            | Vendor::Recraft
            | Vendor::Runway
            | Vendor::Sesame
            | Vendor::Sourceful
            | Vendor::Ai21
            | Vendor::AionLabs
            | Vendor::Allenai
            | Vendor::Amazon
            | Vendor::AnthraciteOrg
            | Vendor::ArceeAi
            | Vendor::Baidu
            | Vendor::Cognitivecomputations
            | Vendor::Cohere
            | Vendor::Deepcogito
            | Vendor::Gryphe
            | Vendor::IbmGranite
            | Vendor::Inception
            | Vendor::Inclusionai
            | Vendor::Inflection
            | Vendor::Kwaipilot
            | Vendor::Mancer
            | Vendor::Meituan
            | Vendor::MetaLlama
            | Vendor::Morph
            | Vendor::NexAgi
            | Vendor::Nousresearch
            | Vendor::Perceptron
            | Vendor::Perplexity
            | Vendor::Poolside
            | Vendor::Rekaai
            | Vendor::Relace
            | Vendor::Sakana
            | Vendor::Sao10k
            | Vendor::Stepfun
            | Vendor::Tencent
            | Vendor::Thedrummer
            | Vendor::Thinkingmachines
            | Vendor::Undi95
            | Vendor::Upstage
            | Vendor::Writer => &[Provider::Openrouter],
        }
    }

    pub const ALL: [Vendor; 66] = [
        Vendor::Anthropic,
        Vendor::Openai,
        Vendor::Google,
        Vendor::Moonshotai,
        Vendor::Qwen,
        Vendor::Deepseek,
        Vendor::Xai,
        Vendor::Zhipu,
        Vendor::Minimax,
        Vendor::Xiaomi,
        Vendor::Openrouter,
        Vendor::Alibaba,
        Vendor::BlackForestLabs,
        Vendor::Bytedance,
        Vendor::BytedanceSeed,
        Vendor::Canopylabs,
        Vendor::Deepgram,
        Vendor::FishAudio,
        Vendor::Heygen,
        Vendor::Hexgrad,
        Vendor::Krea,
        Vendor::Kwaivgi,
        Vendor::Meta,
        Vendor::Microsoft,
        Vendor::Mistralai,
        Vendor::Nvidia,
        Vendor::Recraft,
        Vendor::Runway,
        Vendor::Sesame,
        Vendor::Sourceful,
        Vendor::Ai21,
        Vendor::AionLabs,
        Vendor::Allenai,
        Vendor::Amazon,
        Vendor::AnthraciteOrg,
        Vendor::ArceeAi,
        Vendor::Baidu,
        Vendor::Cognitivecomputations,
        Vendor::Cohere,
        Vendor::Deepcogito,
        Vendor::Gryphe,
        Vendor::IbmGranite,
        Vendor::Inception,
        Vendor::Inclusionai,
        Vendor::Inflection,
        Vendor::Kwaipilot,
        Vendor::Mancer,
        Vendor::Meituan,
        Vendor::MetaLlama,
        Vendor::Morph,
        Vendor::NexAgi,
        Vendor::Nousresearch,
        Vendor::Perceptron,
        Vendor::Perplexity,
        Vendor::Poolside,
        Vendor::Rekaai,
        Vendor::Relace,
        Vendor::Sakana,
        Vendor::Sao10k,
        Vendor::Stepfun,
        Vendor::Tencent,
        Vendor::Thedrummer,
        Vendor::Thinkingmachines,
        Vendor::Undi95,
        Vendor::Upstage,
        Vendor::Writer,
    ];
}

/// Every provider some catalog vendor maps to — i.e. the credential families a
/// subscription can actually be imported for and sold from.
///
/// Derived from the one place a provider's vendor is written down, in the order
/// `providers::PROVIDERS` lists them, which is the order they are offered in.
/// It used to be a third hand-kept list of the same fact, guarded by a test
/// that could only tell you afterwards that you had forgotten it.
pub fn subscribable_providers() -> Vec<Provider> {
    crate::providers::PROVIDERS.iter().filter(|s| s.vendor.is_some()).map(|s| s.provider).collect()
}

/// [`Provider::is_claude_family`] for a wire string, for the call sites that
/// only ever hold one. An id nothing answers to reads as `false`.
pub fn is_claude_family(id: &str) -> bool {
    Provider::from_str_opt(id).is_some_and(|p| p.is_claude_family())
}

/// Whether a provider authenticates with a long-lived API key rather than an
/// OAuth token that expires and is refreshed.
///
/// These accounts are connected by pasting the key, carry no expiry, and are
/// skipped by the refresh loop. Everything downstream (injection, metering, the
/// sell switch) is identical to an OAuth account.
pub fn is_api_key_provider(p: Provider) -> bool {
    matches!(crate::providers::spec(p).credential, crate::providers::Credential::ApiKey { .. })
}

/// Whether connecting this provider runs an RFC 8628 device-code flow rather
/// than the authorization-code + loopback flow the others use.
///
/// Moonshot and xAI both ship their coding subscription through a CLI that
/// authorises by device code: there is no redirect URI to register, the user
/// approves a short code in a browser, and the daemon polls for the token.
pub fn is_device_flow_provider(p: Provider) -> bool {
    crate::providers::spec(p).credential == crate::providers::Credential::DeviceFlow
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
        assert!(!subscribable_providers().contains(&Provider::Custom));
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
            &[
                Provider::Claude,
                Provider::ClaudeWork,
                Provider::ClaudeExtra,
                Provider::Openrouter
            ]
        );
        assert_eq!(Vendor::Openai.providers(), &[Provider::Codex, Provider::Openrouter]);
        assert_eq!(Vendor::Google.providers(), &[Provider::Gemini, Provider::Openrouter]);
        // Both credential kinds serve the same catalog rows; they differ in
        // which host the gateway sends the request to, not in what they can do.
        assert_eq!(
            Vendor::Moonshotai.providers(),
            &[Provider::Kimi, Provider::KimiApi, Provider::Openrouter]
        );
        assert_eq!(Vendor::Qwen.providers(), &[Provider::Qwen, Provider::Openrouter]);
        assert!(is_api_key_provider(Provider::Qwen));
        assert_eq!(
            Vendor::Xai.providers(),
            &[Provider::Xai, Provider::XaiApi, Provider::Openrouter]
        );
        // Every vendor maps to the aggregator as well: one OpenRouter key
        // reaches all of their models, which is exactly what
        // `resells_other_vendors` licenses.
        for v in Vendor::ALL {
            assert!(v.providers().contains(&Provider::Openrouter), "`{v}` is not routable");
        }
        // DeepSeek sells no coding subscription, so there is nothing for the
        // metered key to be told apart from: one vendor, one provider, and the
        // provider keeps the plain vendor name rather than an `_api` suffix.
        assert_eq!(Vendor::Deepseek.providers(), &[Provider::Deepseek, Provider::Openrouter]);
        assert!(is_api_key_provider(Provider::Deepseek));
        assert!(!is_device_flow_provider(Provider::Deepseek));
        assert_eq!(Provider::Deepseek.upstream_family(), Provider::Deepseek);
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
        // A reseller appears once per vendor it serves; the union is a set.
        union.dedup();
        let mut listed = subscribable_providers();
        listed.sort_by_key(|p| p.as_str());
        assert_eq!(union, listed);
    }

    #[test]
    fn claude_work_shares_the_claude_upstream() {
        assert_eq!(Provider::ClaudeWork.upstream_family(), Provider::Claude);
        assert_eq!(Provider::ClaudeExtra.upstream_family(), Provider::Claude);
        assert_eq!(Provider::Codex.upstream_family(), Provider::Codex);
        // The string form the client's `match` arms used to spell out by hand.
        assert!(is_claude_family("claude_extra"));
        assert!(!is_claude_family("codex"));
        assert!(!is_claude_family("nope"));
    }
}

// ── Output modality ─────────────────────────────────────────────────

/// What a model *produces*, which is the same question as "which endpoint
/// serves it".
///
/// Every model traded before this existed answered in text, so the question had
/// one answer and nothing asked it. An aggregator key changes that: the same
/// credential reaches an image model, a video model and a speech model, each on
/// a route of its own with a body shape of its own, and a request sent to the
/// wrong one is a call the upstream bills for and the buyer cannot use. The
/// catalog carries the model's output modalities; this turns them into the one
/// fact both sides branch on.
///
/// Deliberately about *output* only. What a model accepts is already carried
/// separately (`prices.input_modalities`) and does not decide the route: a
/// text→image model and a text+image→image model are both served by
/// `POST /v1/images`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    /// Chat, completions, responses — everything the market traded before.
    Text,
    Image,
    Video,
    /// Text-to-speech: text in, audio bytes out.
    Speech,
    /// Speech-to-text: audio in, a transcript out.
    Transcription,
}

impl Modality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Image => "image",
            Modality::Video => "video",
            Modality::Speech => "speech",
            Modality::Transcription => "transcription",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Modality> {
        Some(match s {
            "text" => Modality::Text,
            "image" => Modality::Image,
            "video" => Modality::Video,
            "speech" => Modality::Speech,
            "transcription" => Modality::Transcription,
            _ => return None,
        })
    }

    /// The path an upstream serves this modality on, relative to the API base.
    ///
    /// These are OpenRouter's own routes and the gateway offers the same ones,
    /// so a caller that can talk to OpenRouter can talk to this platform by
    /// changing the base URL — which is the whole contract of a compatible
    /// gateway. Text has no single answer (a buyer picks chat, completions or
    /// responses) and is not routed from here.
    pub fn upstream_path(&self) -> Option<&'static str> {
        Some(match self {
            Modality::Text => return None,
            Modality::Image => "/images",
            Modality::Video => "/videos",
            Modality::Speech => "/audio/speech",
            Modality::Transcription => "/audio/transcriptions",
        })
    }

    /// Whether a model of this modality answers with JSON a gateway can read,
    /// as opposed to raw bytes it can only hand over.
    ///
    /// Speech is the odd one: `/audio/speech` returns the encoded audio itself,
    /// so there is no `usage` object in the answer and the input side is the
    /// only thing anyone can count.
    pub fn answers_in_json(&self) -> bool {
        !matches!(self, Modality::Speech)
    }

    /// The modality a catalog row's `output_modalities` describes.
    ///
    /// A row listing several — an image model that also emits the text it
    /// reasoned in — is filed under the non-text one: that is the endpoint it
    /// is reached on, and the text is a by-product of the same call. A row
    /// listing nothing is text, which is both the overwhelming majority and
    /// what every row meant before the column existed.
    pub fn of_outputs<S: AsRef<str>>(outputs: &[S]) -> Modality {
        for m in [Modality::Video, Modality::Image, Modality::Speech, Modality::Transcription] {
            if outputs.iter().any(|o| o.as_ref() == m.as_str()) {
                return m;
            }
        }
        Modality::Text
    }

    /// The same answer read off OpenRouter's `architecture.modality` string
    /// (`"text+image->video"`), for rows landed before `output_modalities` was
    /// carried. Nothing to parse reads as text, for the same reason as above.
    pub fn of_modality_str(modality: &str) -> Modality {
        let Some((_, out)) = modality.split_once("->") else { return Modality::Text };
        let parts: Vec<&str> = out.split('+').map(str::trim).collect();
        Modality::of_outputs(&parts)
    }

    pub const ALL: [Modality; 5] = [
        Modality::Text,
        Modality::Image,
        Modality::Video,
        Modality::Speech,
        Modality::Transcription,
    ];
}

impl std::fmt::Display for Modality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod modality_tests {
    use super::*;

    #[test]
    fn a_modality_round_trips_through_its_wire_string() {
        for m in Modality::ALL {
            assert_eq!(Modality::from_str_opt(m.as_str()), Some(m));
            assert_eq!(serde_json::to_value(m).unwrap(), serde_json::json!(m.as_str()));
        }
        assert_eq!(Modality::from_str_opt("hologram"), None);
    }

    #[test]
    fn the_route_follows_the_output_not_the_input() {
        // Both are `POST /v1/images`; what differs is only what they accept.
        assert_eq!(Modality::of_modality_str("text->image"), Modality::Image);
        assert_eq!(Modality::of_modality_str("text+image->image"), Modality::Image);
        assert_eq!(Modality::of_modality_str("text+image->video"), Modality::Video);
        assert_eq!(Modality::of_modality_str("text->speech"), Modality::Speech);
        assert_eq!(Modality::of_modality_str("audio->transcription"), Modality::Transcription);
        assert_eq!(Modality::of_modality_str("text->text"), Modality::Text);
        // A sparse row is text, which is what every row meant before the
        // column existed — never "unroutable", which would silently withdraw
        // capacity that has been selling all along.
        assert_eq!(Modality::of_modality_str(""), Modality::Text);
        assert_eq!(Modality::of_outputs::<&str>(&[]), Modality::Text);
    }

    #[test]
    fn a_model_that_also_emits_text_is_still_filed_under_what_it_is_bought_for() {
        // OpenRouter lists these as `["image", "text"]`, and the endpoint that
        // serves them is `/images` — filing them under text would route an
        // image request at `/chat/completions`.
        assert_eq!(Modality::of_outputs(&["image", "text"]), Modality::Image);
        assert_eq!(Modality::of_outputs(&["text", "audio"]), Modality::Text);
    }

    #[test]
    fn only_text_has_no_route_of_its_own() {
        assert_eq!(Modality::Text.upstream_path(), None);
        for m in Modality::ALL.iter().filter(|m| **m != Modality::Text) {
            assert!(m.upstream_path().is_some_and(|p| p.starts_with('/')), "`{m}` has no route");
        }
        // The one answer nobody can read a token count out of.
        assert!(!Modality::Speech.answers_in_json());
        assert!(Modality::Image.answers_in_json() && Modality::Transcription.answers_in_json());
    }
}
