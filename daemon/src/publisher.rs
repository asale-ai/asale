//! Publisher session wiring (spec §5/§9). Bridges the pure `asale-client-core`
//! wsrelay client to this app's encrypted secret store, local store, and server REST:
//!
//!   - `ConfigSource`   registers the device with the server → device_token,
//!                      resolving fresh credentials on every (re)connect.
//!   - `SupplySource`   builds the supply declaration from imported accounts and
//!                      their real rolling-window quota (plan cap − local usage).
//!   - `TokenProvider`  hands the executor the upstream bearer token, read from
//!                      the encrypted secret store only at injection time (never
//!                      persisted elsewhere, never leaves the device — §5.4/§10.1).
//!   - `RecordSink`     writes per-task metering into `provider_records` (§8).
//!
//! Plus the token-refresh loop (§3.4) that proactively renews access tokens.

use crate::keychain;
use crate::state::AppState;
use asale_client_core::discovery::{self, RefreshedToken, ToolAdapter};
use asale_client_core::protocol::{SupplyItem, Usage};
use asale_protocol::ids::Vendor;
use asale_protocol::providers::CUSTOM_WINDOW_TOKENS;
use asale_client_core::store::LocalStore;
use asale_client_core::{
    spawn_publisher, AccountPool, AccountRuntime, ConfigSource, LeasedToken, PauseReason, PublisherDeps,
    PublisherHandle, Provider, RecordSink, SupplySource, TaskOutcome, TokenProvider, UpstreamErrorKind, Wire,
    WsConfig,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

// ── Sellable model catalog (server-authoritative) ──────────────────────────

/// Settings key holding the last catalog pulled from `/market/models`.
const CATALOG_KEY: &str = "sellable_catalog";

/// How stale the cached catalog may get before it is pulled again. An operator
/// enabling a model on the server should reach sellers within a coffee break,
/// not at the next restart.
const CATALOG_TTL: i64 = 600;

/// How stale the market's prices may get before they are pulled again.
///
/// The server reprices once a minute, and the price is what decides whether an
/// account's band is satisfied — so this is the resolution at which a lane can
/// react to the market at all. It is a much cheaper read than the catalog above
/// (`/market/ratios` is one Postgres query and no Redis), which is why the two
/// have separate clocks rather than one pull doing both.
const PRICE_TTL: i64 = 60;

/// What this device may advertise, per local provider, as of `fetched_at`, and
/// what the market currently pays for it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct SellableCatalog {
    fetched_at: i64,
    by_provider: std::collections::BTreeMap<String, Vec<String>>,
    /// What the market pays per model, in whole percent **of** the vendor's
    /// list price (100 = list price). Empty until the first pull; a model
    /// missing from it has no known price, which is not the same as a price of
    /// zero and must never be judged against a band.
    #[serde(default)]
    ratios: std::collections::BTreeMap<String, i32>,
    /// When those prices were read. Kept apart from `fetched_at` because the
    /// two are refreshed on different clocks.
    #[serde(default)]
    priced_at: i64,
}

/// Fallback advertised models, used only until the first successful catalog
/// pull (a fresh install that starts offline would otherwise sell nothing).
///
/// These are native vendor API ids, because the gateway relays the id a
/// consumer asked for verbatim — see `native_model_name` on the server.
fn fallback_models(provider: &str) -> &'static [&'static str] {
    Provider::from_str_opt(provider).map_or(&[][..], |p| asale_protocol::spec(p).fallback_models)
}

/// Model ids a vendor's own API is known to answer to, when that set is
/// narrower than the catalog's.
///
/// The catalog's right-hand slug is OpenRouter's spelling, and for xAI it is
/// not always the vendor's: OpenRouter lists `grok-4.20`, while xAI's own API
/// serves `grok-4.20-0309-reasoning` and `grok-4.20-0309-non-reasoning`.
/// Relaying `grok-4.20` therefore fails *after* the request has been matched,
/// preauthorized and routed — the publisher wears a failure that was never
/// its fault. Rewriting the id would mean guessing which of the two variants a
/// consumer meant, so instead the mismatched rows are simply not advertised.
///
/// DeepSeek is the same problem from the other direction. Its API accepts
/// exactly two model strings — `deepseek-v4-flash` and `deepseek-v4-pro`, each
/// a pointer the vendor moves to its newest re-post ("simply use
/// `deepseek-v4-flash` or `deepseek-v4-pro` to access the latest version",
/// <https://api-docs.deepseek.com/quick_start/models>). The catalog carries far
/// more than that under the same vendor: the dated re-posts the pointers
/// resolve to (`deepseek-v4-flash-0731`), and the whole V3/R1 back catalogue
/// the aggregator can still route elsewhere. All of them are real rows worth
/// pricing — a custom endpoint may well serve them — and none of them is a
/// string a DeepSeek key can send.
///
/// `None` means "advertise whatever the catalog lists", which is the case for
/// every other vendor including Moonshot, whose ids line up exactly.
///
/// Source: the model registry the vendor CLIs ship, read from CLIProxyAPI
/// `internal/registry/models/models.json`.
fn native_models(provider: &str) -> Option<&'static [&'static str]> {
    Provider::from_str_opt(provider).and_then(|p| asale_protocol::spec(p).native_models)
}

/// Catalog vendor (`prices.provider`, an OpenRouter vendor slug) → the local
/// providers whose subscriptions can serve it.
///
/// The mapping itself lives in `asale-protocol` (`Vendor::providers`) because
/// the server's catalog writes those same slugs. Unknown slugs — including
/// OpenRouter's own `~vendor` routing aliases (`claude-opus-latest`) — map to
/// nothing on purpose: they are not ids any vendor API accepts, so relaying one
/// would fail upstream.
fn providers_for_vendor(vendor: &str) -> &'static [Provider] {
    match Vendor::from_str_opt(vendor) {
        Some(v) => v.providers(),
        None => &[],
    }
}

/// Whether a catalog row answers in text. A subscription relays chat traffic;
/// an image/audio endpoint is a different API surface the executor cannot
/// serve. An unknown modality is treated as text rather than dropped, so a
/// sparse catalog row does not silently remove sellable capacity.
fn produces_text(modality: &str) -> bool {
    match modality.split_once("->") {
        Some((_, out)) => out.contains("text"),
        None => true,
    }
}

/// Models this provider may advertise right now.
///
/// The server's `prices` table is the only authority on what can be traded —
/// `gateway::relay` refuses to relay a model it has no enabled price row for,
/// so anything advertised beyond that list is capacity no consumer can buy.
/// A cached answer is used even when it is empty: "the platform trades nothing
/// this subscription can serve" is a real answer, and falling back to the
/// built-in list there would re-create the very mismatch this replaces.
/// `entitled` narrows the list to what one *account* may ask its upstream for,
/// where that is narrower still than what the vendor's API serves in general
/// (Codex, see [`codex_entitlement`]). `None` means no such limit applies;
/// `Some(&[])` means the account is granted nothing and must advertise nothing —
/// which is why the caller, not this function, decides what an unknown
/// entitlement means.
/// What an endpoint may advertise out of what it serves.
///
/// `chat_only` is a custom endpoint on the OpenAI schema whose host has no
/// `/responses` route. It still lists the responses-only models — an aggregator
/// lists everything it proxies — but it cannot serve one: the moment a buyer's
/// request carries a tool the upstream answers 400, which costs that buyer a
/// turn and this device its reputation. So they are not offered. An endpoint
/// that does have the route offers them there instead
/// (`AccountRuntime::wire_for`).
fn offerable(served: Vec<String>, chat_only: bool) -> Vec<String> {
    match chat_only {
        false => served,
        true => served.into_iter().filter(|m| !asale_client_core::pool::needs_responses_wire(m)).collect(),
    }
}

fn sellable_models(catalog: &Option<SellableCatalog>, provider: &str, entitled: Option<&[String]>) -> Vec<String> {
    let listed: Vec<String> = match catalog {
        // A custom endpoint is not tied to one vendor's credential family, so
        // the catalog has no column for it: what it *may* sell is everything the
        // platform trades, and what it *can* sell is narrowed by the endpoint's
        // own model list, which arrives as `entitled` below.
        //
        // Alibaba's Model Studio is the same shape wearing a vendor's name. One
        // key there serves DeepSeek, Kimi, GLM and the rest alongside Alibaba's
        // own models, so filing it under `qwen/*` — the vendor its *credential*
        // belongs to — hid every model it resells. Each of those keeps the
        // vendor the catalog files it under, so a DeepSeek model sold through a
        // Model Studio key is still a DeepSeek row on the board; only the lane's
        // credential family is `qwen`. What stops this widening into models the
        // key cannot serve is the same thing that stops it for a custom
        // endpoint: `entitled` is the vendor's own `/models`.
        Some(c) if Provider::from_str_opt(provider).is_some_and(asale_protocol::providers::resells_other_vendors) => {
            let mut all: Vec<String> = c.by_provider.values().flatten().cloned().collect();
            all.sort();
            all.dedup();
            all
        }
        Some(c) => c.by_provider.get(provider).cloned().unwrap_or_default(),
        // A custom endpoint has no built-in model set to fall back on — its
        // models are whatever its operator's endpoint serves — so before the
        // first catalog pull it advertises nothing rather than guessing.
        None => fallback_models(provider).iter().map(|s| s.to_string()).collect(),
    };
    let listed: Vec<String> = match entitled {
        Some(granted) => listed.into_iter().filter(|m| granted.contains(m)).collect(),
        None => listed,
    };
    match native_models(provider) {
        Some(native) => listed.into_iter().filter(|m| native.contains(&m.as_str())).collect(),
        None => listed,
    }
}

/// Settings key holding one Codex account's entitled slugs.
fn codex_entitlement_key(account_id: &str) -> String {
    format!("codexmodels:{account_id}")
}

/// Settings key holding when this account's last entitlement lookup failed.
fn codex_entitlement_retry_key(account_id: &str) -> String {
    format!("codexmodels:failed:{account_id}")
}

/// How stale a cached entitlement may get. The set changes when OpenAI ships a
/// Codex release or the owner's plan changes, so an hour is soon enough — and
/// it keeps this off the hot path of the periodic pool rebuild.
const CODEX_ENTITLEMENT_TTL: i64 = 3600;

/// How long a *failed* lookup suppresses the next attempt.
///
/// The sell page rebuilds the pool on every `list_accounts` / `list_lanes`, so
/// on a network that cannot reach the Codex backend at all, every render used to
/// add one more upstream call that had to time out before the page could finish
/// — which is what the user sees as a sell page stuck on its skeleton. A minute
/// is short enough that a proxy coming up is picked up promptly, and long enough
/// that the UI stops paying for the outage.
const CODEX_ENTITLEMENT_RETRY_BACKOFF: i64 = 60;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct CodexEntitlement {
    fetched_at: i64,
    models: Vec<String>,
}

/// The slugs this Codex account may ask its upstream for.
///
/// A ChatGPT subscription is entitled to a per-account, per-plan slice of
/// OpenAI's models, and the Codex backend refuses everything outside it with
/// `400 … "is not supported when using Codex with a ChatGPT account"` — so the
/// catalog alone is not enough to know what this account can sell. Asking the
/// account itself is the only way (`discovery::codex_servable_models`).
///
/// An empty answer is returned as such, and the caller advertises nothing for
/// this account: relaying a slug the upstream will refuse costs the consumer a
/// failed turn and this device a reputation hit, so silence is the better trade.
/// A failed *call* is different from an empty answer — the last successful list
/// is kept however old it is, because a network blip must not take a working
/// publisher off the market.
async fn codex_entitlement(store: &LocalStore, tool: &asale_client_core::store::ToolRow) -> Vec<String> {
    let key = codex_entitlement_key(&tool.account_id);
    let cached: Option<CodexEntitlement> = store
        .get_setting(&key)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    if let Some(c) = &cached {
        if now_secs() - c.fetched_at < CODEX_ENTITLEMENT_TTL {
            return c.models.clone();
        }
    }
    let stale = || cached.clone().map(|c| c.models).unwrap_or_default();
    let retry_key = codex_entitlement_retry_key(&tool.account_id);
    let failed_at = store
        .get_setting(&retry_key)
        .await
        .ok()
        .flatten()
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0);
    if now_secs() - failed_at < CODEX_ENTITLEMENT_RETRY_BACKOFF {
        return stale();
    }
    let Some(token) = keychain::get(&tool.keychain_ref).ok().flatten() else {
        tracing::warn!(account = %tool.account_id, "codex entitlement: no token in the secret store");
        return stale();
    };
    let upstream_id = store
        .get_setting(&upstream_acct_key(&tool.provider, &tool.account_id))
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .or_else(|| asale_client_core::executor::chatgpt_account_id(&token))
        .unwrap_or_default();
    match asale_client_core::discovery::codex_servable_models(&token, &upstream_id).await {
        Ok(models) => {
            let fresh = CodexEntitlement { fetched_at: now_secs(), models };
            if cached.as_ref().map(|c| &c.models) != Some(&fresh.models) {
                tracing::info!(account = %tool.account_id, "codex account serves {:?}", fresh.models);
            }
            if let Ok(raw) = serde_json::to_string(&fresh) {
                let _ = store.set_setting(&key, &raw).await;
            }
            let _ = store.set_setting(&retry_key, "0").await;
            fresh.models
        }
        Err(e) => {
            tracing::warn!(account = %tool.account_id, "codex entitlement lookup failed: {e}");
            let _ = store.set_setting(&retry_key, &now_secs().to_string()).await;
            stale()
        }
    }
}

/// The provider id a custom endpoint account is stored under.
pub const CUSTOM_PROVIDER: &str = "custom";

/// Settings key holding one custom account's endpoint.
///
/// The base URL lives beside the account rather than in the `tools` row because
/// it is not a property every account has — every other provider's upstream is
/// the vendor's and is known at compile time.
pub fn custom_base_key(account_id: &str) -> String {
    format!("custombase:{account_id}")
}

/// Settings key holding the dialect one custom endpoint speaks.
///
/// Beside the base URL and for the same reason — it is a property of that one
/// account, not of its provider. Absent (an endpoint connected before the
/// dialect was a choice) reads as [`Wire::Openai`], which is what it was.
pub fn custom_wire_key(account_id: &str) -> String {
    format!("customwire:{account_id}")
}

/// Settings key recording whether one custom endpoint also serves `/responses`.
///
/// Beside the dialect and for the same reason — one endpoint can serve both
/// OpenAI routes, and which models need which is not a property of the account
/// but of the model (`pool::needs_responses_wire`). Absent means nobody has
/// asked yet, which is what triggers the probe below; `0` is an answer.
pub fn custom_responses_key(account_id: &str) -> String {
    format!("customresponses:{account_id}")
}

/// The dialect this custom account's endpoint speaks, as recorded.
pub async fn custom_wire(store: &LocalStore, account_id: &str) -> Wire {
    store
        .get_setting(&custom_wire_key(account_id))
        .await
        .ok()
        .flatten()
        .and_then(|w| Wire::from_str_opt(&w))
        .unwrap_or_default()
}

/// Whether this custom endpoint's host serves the Responses route, as recorded
/// by the probe. Unknown reads as "no": the account keeps the one route it is
/// already known to work on.
pub async fn custom_has_responses(store: &LocalStore, account_id: &str) -> bool {
    store
        .get_setting(&custom_responses_key(account_id))
        .await
        .ok()
        .flatten()
        .is_some_and(|v| v == "1")
}

/// Settings key holding the model list one custom endpoint last reported.
fn custom_models_key(account_id: &str) -> String {
    format!("custommodels:{account_id}")
}

/// What one custom endpoint serves, keyed the way the market names it.
///
/// The two spellings are rarely the same. An aggregator lists
/// `anthropic/claude-haiku-4.5`; the platform trades `claude-haiku-4-5`, because
/// that is the id a vendor's own API answers to and the catalog stores the
/// native form (server `catalog::native_model_name`). So the market id is what
/// the lane is declared under and matched on, and the endpoint id is what has to
/// travel in the request body — hence a map rather than a list.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CustomListing {
    pub fetched_at: i64,
    /// market model id -> the id this endpoint knows it by.
    pub aliases: std::collections::BTreeMap<String, String>,
}

impl CustomListing {
    /// The market ids this endpoint can serve, for the catalog intersection.
    fn market_ids(&self) -> Vec<String> {
        self.aliases.keys().cloned().collect()
    }
}

/// The market id for a model as one endpoint spells it.
///
/// Mirrors the server's own normalisation so the two sides agree on what a model
/// is called: the vendor prefix is dropped (the catalog stores the right-hand
/// half), and Anthropic's dotted versions become hyphenated, which is the only
/// vendor whose published id and API id differ that way.
///
/// A plain id with no prefix is already a market id — that is how a vendor's own
/// API and most self-hosted gateways list their models.
fn market_id_of(endpoint_id: &str) -> String {
    match endpoint_id.split_once('/') {
        // `~anthropic` is the catalog's alias row for the same vendor.
        Some((vendor, model)) if vendor.trim_start_matches('~') == "anthropic" => model.replace('.', "-"),
        Some((_, model)) => model.to_string(),
        None => endpoint_id.to_string(),
    }
}

/// The market ids one endpoint id could plausibly be, most literal first.
///
/// Endpoints spell the same model in several ways and none of them is wrong:
/// an aggregator prefixes the vendor (`anthropic/claude-haiku-4.5`), a gateway
/// that fronts one account does not (`claude-haiku-4.5`, `gpt-5.2`), and
/// Anthropic's own published versions are dotted while its API — and therefore
/// the platform's catalog — is hyphenated (`claude-haiku-4-5`).
///
/// Guessing the vendor from the id is what this replaces, and it was wrong in a
/// way that lost a whole family: an unprefixed `claude-opus-4.5` looks like no
/// vendor at all, so the dot rule never fired and every Claude model on such an
/// endpoint failed to match the catalog. Trying the forms and letting the
/// catalog decide needs no guess.
fn market_candidates(endpoint_id: &str) -> Vec<String> {
    let bare = endpoint_id.split_once('/').map(|(_, m)| m).unwrap_or(endpoint_id);
    let mut out = vec![endpoint_id.to_string(), bare.to_string()];
    if bare.contains('.') {
        out.push(bare.replace('.', "-"));
    }
    // And the same forms lowercased, last so a literal match still wins. Model
    // Studio lists the models it resells under the reseller's own casing
    // (`ZHIPU/GLM-5.3`, `MiniMax/MiniMax-M3`) while the catalog — like every
    // vendor's own API — spells them lower. Without this the newest GLM and
    // MiniMax flagships matched nothing and were silently unsellable.
    let lowered: Vec<String> =
        out.iter().map(|c| c.to_lowercase()).filter(|c| !out.contains(c)).collect();
    out.extend(lowered);
    out.dedup();
    out
}

/// Index an endpoint's model list by the id the market knows each model as.
///
/// `tradable` is what the platform actually prices — the catalog — and it is
/// the judge: the first candidate spelling that appears in it wins, and an id
/// with no match is left out entirely, because a lane for a model the market
/// does not trade can never earn.
///
/// Before the first catalog pull `tradable` is empty and there is nothing to
/// judge against. The list is kept anyway, under the best guess ([`market_id_of`]),
/// rather than dropped: `sellable_models` intersects with the catalog again once
/// there is one, so a wrong guess costs nothing, while dropping everything would
/// leave a freshly connected endpoint advertising nothing until the next hourly
/// refresh.
///
/// Collisions are real, not hypothetical, and which side wins matters. Model
/// Studio lists the models it resells twice — `glm-5.2` *and* `ZHIPU/GLM-5.2`,
/// `MiniMax-M2.5` *and* `MiniMax/MiniMax-M2.5` — and the two are not
/// interchangeable: the bare id is the product Alibaba hosts, while the
/// prefixed one is a marketplace listing that answers `400 ... product is not
/// activated` until the account subscribes to it separately. Sorting plain
/// ASCII put the uppercase reseller spelling first and picked the id most
/// accounts cannot call.
///
/// So an unprefixed id wins, and ties below that are broken by name, which
/// keeps the choice stable across rebuilds rather than dependent on the
/// endpoint's ordering.
pub fn index_custom_models(
    endpoint_ids: &[String],
    tradable: &[String],
) -> std::collections::BTreeMap<String, String> {
    let mut sorted: Vec<&String> = endpoint_ids.iter().collect();
    sorted.sort_by_key(|id| (id.contains('/'), id.as_str()));
    let mut out = std::collections::BTreeMap::new();
    for id in sorted {
        let market = match market_candidates(id).into_iter().find(|c| tradable.iter().any(|t| t == c)) {
            Some(m) => m,
            None if tradable.is_empty() => market_id_of(id),
            None => continue,
        };
        out.entry(market).or_insert_with(|| id.clone());
    }
    out
}

/// Every model the platform trades, from the cached catalog. Empty before the
/// first pull, which callers read as "no opinion" rather than "nothing".
async fn tradable_models(store: &LocalStore) -> Vec<String> {
    let Some(c) = load_catalog(store).await else { return Vec::new() };
    let mut all: Vec<String> = c.by_provider.values().flatten().cloned().collect();
    all.sort();
    all.dedup();
    all
}

/// Record what a custom endpoint serves, as of now.
///
/// Called when an endpoint is connected, so the account is sellable from the
/// probe's answer instead of only after the next rebuild goes and asks again.
/// It writes the same cache [`custom_endpoint_models`] reads, so the TTL and the
/// staleness rules stay in one place.
pub async fn store_custom_models(
    store: &LocalStore,
    account_id: &str,
    endpoint_ids: &[String],
) -> anyhow::Result<CustomListing> {
    let tradable = tradable_models(store).await;
    let listing =
        CustomListing { fetched_at: now_secs(), aliases: index_custom_models(endpoint_ids, &tradable) };
    store.set_setting(&custom_models_key(account_id), &serde_json::to_string(&listing)?).await?;
    Ok(listing)
}

/// How stale a custom endpoint's model list may get before it is re-read. The
/// same hour Codex's entitlement uses, for the same reason: the set moves when
/// somebody reconfigures the endpoint, which is not something to ask about on
/// every pool rebuild.
const CUSTOM_MODELS_TTL: i64 = 3600;

/// Whatever model list is cached for one account, however stale.
async fn cached_listing(store: &LocalStore, account_id: &str) -> Option<CustomListing> {
    store
        .get_setting(&custom_models_key(account_id))
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

/// The models one account's own endpoint currently serves, by market id.
///
/// Cached and refreshed on the same terms as [`codex_entitlement`]: a stale list
/// outlives a failed call, because a network blip must not take a working
/// publisher off the market, while an empty *answer* is taken at face value.
async fn endpoint_models(
    store: &LocalStore,
    tool: &asale_client_core::store::ToolRow,
    base: &str,
    wire: Wire,
) -> CustomListing {
    let cached = cached_listing(store, &tool.account_id).await;
    if let Some(c) = &cached {
        if now_secs() - c.fetched_at < CUSTOM_MODELS_TTL {
            return c.clone();
        }
    }
    let stale = || cached.clone().unwrap_or_default();
    let Some(key_value) = keychain::get(&tool.keychain_ref).ok().flatten() else {
        tracing::warn!(account = %tool.account_id, provider = %tool.provider, "no key in the secret store");
        return stale();
    };
    match asale_client_core::discovery::custom_endpoint_models(base, &key_value, wire).await {
        Ok(models) => match store_custom_models(store, &tool.account_id, &models).await {
            Ok(fresh) => {
                if cached.as_ref().map(|c| &c.aliases) != Some(&fresh.aliases) {
                    tracing::info!(
                        account = %tool.account_id,
                        provider = %tool.provider,
                        "endpoint serves {} models ({} usable ids)",
                        models.len(),
                        fresh.aliases.len()
                    );
                }
                fresh
            }
            Err(e) => {
                tracing::warn!(account = %tool.account_id, "storing the model list failed: {e}");
                stale()
            }
        },
        Err(e) => {
            tracing::warn!(account = %tool.account_id, provider = %tool.provider, "model lookup failed: {e}");
            stale()
        }
    }
}

/// The models one custom endpoint currently serves. Its host and its dialect
/// are its operator's, so both come out of the account's own settings.
async fn custom_endpoint_models(store: &LocalStore, tool: &asale_client_core::store::ToolRow) -> CustomListing {
    let base = match store.get_setting(&custom_base_key(&tool.account_id)).await.ok().flatten() {
        Some(b) if !b.is_empty() => b,
        _ => {
            tracing::warn!(account = %tool.account_id, "custom account has no base URL recorded");
            return cached_listing(store, &tool.account_id).await.unwrap_or_default();
        }
    };
    let wire = custom_wire(store, &tool.account_id).await;
    // Backfill for an account connected before the second route was a question.
    // Runs once — the answer, either way, is stored — and only for the schema
    // that has two routes to choose between.
    if wire == Wire::Openai && store.get_setting(&custom_responses_key(&tool.account_id)).await.ok().flatten().is_none() {
        if let Some(key) = keychain::get(&tool.keychain_ref).ok().flatten() {
            let has = asale_client_core::discovery::custom_endpoint_has_responses(&base, &key).await;
            tracing::info!(account = %tool.account_id, "endpoint {} the responses route", if has { "serves" } else { "does not serve" });
            let _ = store.set_setting(&custom_responses_key(&tool.account_id), if has { "1" } else { "0" }).await;
        }
    }
    endpoint_models(store, tool, &base, wire).await
}

/// The models one *vendor* API key may actually ask for, from the vendor's own
/// `/models`.
///
/// The tradable catalog and its reference prices come from OpenRouter, whose
/// `qwen/*` rows are not the set Alibaba serves: of the 50 it prices, 20 are
/// third-party re-hosts DashScope has never answered to under that spelling —
/// `qwen-2.5-72b-instruct` (Alibaba writes `qwen2.5-…`, no hyphen), every
/// `qwen3-vl-*`, `qwen3-coder`, `qwen3-max-thinking`. A lane behind one of those
/// 404s on its first request, which costs a consumer a turn and this device its
/// reputation — the same failure `0066_disable_variant_catalog_ids` was written
/// for.
///
/// So the vendor is the authority on what its key may offer, exactly as a custom
/// endpoint's operator is. A list hardcoded in the table would say the same thing
/// today and would have to be re-released every time Alibaba ships a model.
async fn vendor_endpoint_models(
    store: &LocalStore,
    tool: &asale_client_core::store::ToolRow,
    p: Provider,
) -> CustomListing {
    let spec = asale_protocol::spec(p);
    endpoint_models(store, tool, spec.api_base, spec.wire).await
}

/// The cached catalog, or None when nothing has been pulled yet.
async fn load_catalog(store: &LocalStore) -> Option<SellableCatalog> {
    store
        .get_setting(CATALOG_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<SellableCatalog>(&raw).ok())
        .filter(|c| c.fetched_at > 0)
}

/// Pull the tradable catalog and cache it. Returns whether the model set moved,
/// which is the caller's cue to rebuild the pool and re-declare.
pub async fn refresh_sellable_catalog(store: &LocalStore, api_base: &str) -> anyhow::Result<bool> {
    let http = asale_client_core::http::plain();
    let resp = http.get(format!("{api_base}/api/v1/market/models")).send().await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    let rows = body
        .get("models")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow::anyhow!("market/models returned {status}: {body}"))?;

    let mut by_provider: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for row in rows {
        let model = row.get("model").and_then(|v| v.as_str()).unwrap_or_default();
        let vendor = row.get("provider").and_then(|v| v.as_str()).unwrap_or_default();
        let modality = row.get("modality").and_then(|v| v.as_str()).unwrap_or_default();
        if model.is_empty() || !produces_text(modality) {
            continue;
        }
        // A custom endpoint is not tied to one of the built-in subscription
        // families, so keep every text model in its candidate pool — including
        // the ones whose catalog vendor maps to no provider at all, which are
        // otherwise filed under nothing and can never be sold. The endpoint's
        // own /models narrows this list before anything is advertised, so it
        // cannot widen an ordinary subscription lane.
        by_provider
            .entry(CUSTOM_PROVIDER.to_string())
            .or_default()
            .push(model.to_string());
        for p in providers_for_vendor(vendor) {
            by_provider.entry(p.as_str().to_string()).or_default().push(model.to_string());
        }
    }
    for list in by_provider.values_mut() {
        list.sort();
        list.dedup();
    }

    let previous = store
        .get_setting(CATALOG_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<SellableCatalog>(&raw).ok())
        .unwrap_or_default();
    let changed = previous.by_provider != by_provider;
    // The prices ride along in the same record and are refreshed on their own,
    // much faster clock — so carry them over rather than blanking every band
    // verdict on the device every ten minutes.
    let fresh = SellableCatalog {
        fetched_at: now_secs(),
        by_provider,
        ratios: previous.ratios,
        priced_at: previous.priced_at,
    };
    store.set_setting(CATALOG_KEY, &serde_json::to_string(&fresh)?).await?;
    if changed {
        tracing::info!("sellable catalog updated: {:?}", fresh.by_provider);
    }
    Ok(changed)
}

/// Pull what the market currently pays for every tradable model and cache it.
///
/// This is what the per-account price bands are judged against. It deliberately
/// does *not* fail the caller when the market is unreachable: a lane with no
/// known price keeps selling on the terms it already had (see
/// `pool::apply_price_band`), so an outage here costs freshness, not supply.
///
/// `/market/ratios` is the endpoint built for this — one Postgres query, no
/// Redis, safe to poll once a minute per device. A server that predates it
/// falls back to `/market/models`, which carries the same `ratio` field along
/// with the whole market board. The fallback matters more than it looks: the
/// client and the server ship separately, and without it every seller on an
/// older deployment reads no price at all — which is indistinguishable, on
/// screen, from a market that pays nothing.
pub async fn refresh_market_prices(store: &LocalStore, api_base: &str) -> anyhow::Result<()> {
    let http = asale_client_core::http::plain();
    let mut rows = fetch_ratio_rows(&http, &format!("{api_base}/api/v1/market/ratios")).await;
    if rows.is_err() {
        let fallback = fetch_ratio_rows(&http, &format!("{api_base}/api/v1/market/models")).await;
        if fallback.is_ok() {
            tracing::debug!("market/ratios unavailable; read prices from market/models instead");
            rows = fallback;
        }
    }

    let mut ratios: std::collections::BTreeMap<String, i32> = Default::default();
    for row in rows? {
        let model = row.get("model").and_then(|v| v.as_str()).unwrap_or_default();
        // `ratio` is the fraction of list price, in [0.10, 1.00]. A row without
        // one says nothing about the price and is skipped rather than defaulted
        // — a missing price and a price of zero are not the same claim.
        let Some(ratio) = row.get("ratio").and_then(serde_json::Value::as_f64) else {
            continue;
        };
        if model.is_empty() {
            continue;
        }
        ratios.insert(model.to_string(), (ratio * 100.0).round().clamp(0.0, 100.0) as i32);
    }
    if ratios.is_empty() {
        anyhow::bail!("the market returned no prices");
    }

    let mut catalog = store
        .get_setting(CATALOG_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<SellableCatalog>(&raw).ok())
        .unwrap_or_default();
    catalog.ratios = ratios;
    catalog.priced_at = now_secs();
    store.set_setting(CATALOG_KEY, &serde_json::to_string(&catalog)?).await?;
    Ok(())
}

/// GET a market endpoint and hand back its `models` array. Both endpoints that
/// carry prices answer in this shape.
async fn fetch_ratio_rows(
    http: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let resp = http.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}");
    }
    let body: serde_json::Value = resp.json().await?;
    body.get("models")
        .and_then(|m| m.as_array())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{url} returned no models array: {body}"))
}

/// Whether the cached catalog is due for a pull.
async fn catalog_is_stale(store: &LocalStore) -> bool {
    let fetched_at = load_catalog(store).await.map(|c| c.fetched_at).unwrap_or(0);
    now_secs() - fetched_at >= CATALOG_TTL
}

/// Whether the cached market prices are due for a pull.
async fn prices_are_stale(store: &LocalStore) -> bool {
    let priced_at = load_catalog(store).await.map(|c| c.priced_at).unwrap_or(0);
    now_secs() - priced_at >= PRICE_TTL
}

// ── Publisher policy (server-authoritative, mirrored locally) ──────────────

/// Local mirror of the server's `PublishConfig` (`GET/PUT /me/publish-config`).
///
/// The server owns this: it is applied there at every supply.declare and is
/// what a fresh install, a second device, or the operator sees. The mirror
/// exists so the client can render and pre-apply the policy while offline —
/// never as a second source of truth. Only `min_price` is applied locally
/// (as the declared floor); the reserve floor and the model lists are left to
/// the server, or they would be applied twice.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PublishPolicy {
    #[serde(default)]
    pub reserve_floor: i64,
    #[serde(default)]
    pub max_rpm: i32,
    #[serde(default)]
    pub min_price: i64,
    #[serde(default)]
    pub model_whitelist: Vec<String>,
    #[serde(default)]
    pub model_blacklist: Vec<String>,
}

/// Settings key holding the mirrored policy JSON.
pub const POLICY_KEY: &str = "publish_policy";

/// The mirrored policy, or defaults when it has never been pulled.
pub async fn local_policy(store: &LocalStore) -> PublishPolicy {
    let raw = store.get_setting(POLICY_KEY).await.ok().flatten().unwrap_or_default();
    let mut policy: PublishPolicy = serde_json::from_str(&raw).unwrap_or_default();
    // Pre-mirror installs kept the price floor in its own setting; honour it
    // until the first pull replaces the whole policy.
    if policy.min_price == 0 {
        if let Ok(Some(legacy)) = store.get_setting("publish_price_min").await {
            policy.min_price = legacy.parse().unwrap_or(0);
        }
    }
    policy
}

/// Replace the mirrored policy.
pub async fn store_policy(store: &LocalStore, policy: &PublishPolicy) -> anyhow::Result<()> {
    store.set_setting(POLICY_KEY, &serde_json::to_string(policy)?).await?;
    Ok(())
}

/// Build the adapter for a provider using the resolved public client ids.
pub fn adapter_for(provider: &str) -> Option<Box<dyn ToolAdapter>> {
    match provider {
        p if asale_protocol::ids::is_claude_family(p) => Some(Box::new(discovery::ClaudeAdapter::new(
            Provider::from_str_opt(p)?,
            crate::oauth::claude_client_id(),
        ))),
        "codex" => Some(Box::new(discovery::CodexAdapter::new(crate::oauth::codex_client_id()))),
        "gemini" => Some(Box::new(discovery::GeminiAdapter::new(
            crate::oauth::gemini_client_id(),
            crate::oauth::gemini_client_secret(),
        ))),
        // Kimi Code / Grok CLI (device flow) and the two platform APIs (pasted
        // key, nothing to refresh). Both resolve from the provider id alone.
        other => {
            let p = Provider::from_str_opt(other)?;
            discovery::DeviceFlowAdapter::for_provider(p)
                .map(|a| Box::new(a) as Box<dyn ToolAdapter>)
                .or_else(|| {
                    discovery::ApiKeyAdapter::for_provider(p).map(|a| Box::new(a) as Box<dyn ToolAdapter>)
                })
        }
    }
}

// ── ConfigSource: register device → device_token for the WS handshake ───────

struct RestConfigSource {
    server_api_base: String,
    gateway_ws_url: String,
    device_id: String,
    device_pubkey: String,
}

#[async_trait]
impl ConfigSource for RestConfigSource {
    async fn ws_config(&self) -> anyhow::Result<WsConfig> {
        let mut access = keychain::get("access_token")?.ok_or_else(|| anyhow::anyhow!("not signed in"))?;
        let http = asale_client_core::http::plain();
        // Same 401-then-refresh-once shape as `commands::server_client::authed`.
        // Without it an access token that merely expired parks the reconnect
        // loop on a dead token: it never refreshes, so it retries the same
        // rejected credential until the app restarts.
        for attempt in 0..2 {
            let resp = http
                .post(format!("{}/api/v1/devices", self.server_api_base))
                .header("authorization", format!("Bearer {access}"))
                .json(&json!({"device_id": self.device_id, "device_pubkey": self.device_pubkey}))
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                access = crate::commands::server_client::refresh_access_token(&self.server_api_base, &http)
                    .await
                    .map_err(|e| anyhow::anyhow!("device registration failed: {e}"))?;
                continue;
            }
            let v: serde_json::Value = resp.json().await?;
            let device_token = v
                .get("device_token")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("device registration failed: {v}"))?
                .to_string();
            return Ok(WsConfig {
                gateway_ws_url: self.gateway_ws_url.clone(),
                device_id: self.device_id.clone(),
                device_token,
            });
        }
        unreachable!("ws_config loop always returns")
    }
}

// ── SupplySource: the pool's lanes → supply items ───────────────────────────

/// Builds the declaration from the account pool, one entry per `(provider,
/// model)` lane.
///
/// The pool — not the `tools` table — is the source here because it is the only
/// place that knows the *runtime* half of a lane's state: what is cooling after
/// an upstream error, what the breaker has taken out, what a rate limit
/// suspended until when. `rebuild_pool` keeps its stored half (sell switches,
/// quota, daily caps) current, so reading one structure gives the whole
/// picture.
///
/// A lane that cannot serve is still declared, with `available: false` and the
/// reason. Dropping it instead would leave the seller staring at an empty
/// market board with no explanation, and the gateway with no way to tell
/// "withheld" from "this device fell over".
struct PoolSupplySource {
    store: Arc<LocalStore>,
    pool: Arc<StdMutex<AccountPool>>,
}

/// What all of one provider+model's accounts add up to.
#[derive(Default)]
struct LaneOffer {
    window_remaining: i64,
    concurrency_free: i32,
    /// Set only when nothing can serve the lane; the reason to show.
    pause: Option<(String, i64)>,
    /// Lowest floor among the accounts *serving* this lane, in whole percent of
    /// list price. `None` until one of them is actually selling.
    ///
    /// Only the serving accounts count. An account holding the lane back has an
    /// opinion about the price but no capacity behind it, and quoting a floor
    /// nothing can serve would let an idle account set the market's price.
    ask_ratio: Option<i64>,
    /// Marks for the credentials behind this lane, one per contributing
    /// account, collected in a set so the declaration does not change when the
    /// accounts happen to be iterated in a different order.
    credentials: std::collections::BTreeSet<String>,
}

#[async_trait]
impl SupplySource for PoolSupplySource {
    async fn declare_items(&self) -> serde_json::Value {
        build_supply_items(&self.store, &self.pool).await
    }
}

/// The declaration this device would send right now, as a JSON array of
/// `SupplyItem`. Split out from the trait so it can be exercised directly.
pub async fn build_supply_items(store: &LocalStore, pool: &StdMutex<AccountPool>) -> serde_json::Value {
    let price_min: i64 = local_policy(store).await.min_price;
    let now = now_secs();
    // The dialect each lane is offered in, decided once and used both to pick
    // whose capacity counts towards it below and to pick who serves it when the
    // work arrives (`PoolTokens::acquire`). Empty for every provider whose
    // upstream is the vendor's, which settles the dialect on its own.
    let (views, lane_wires) = match pool.lock() {
        Ok(p) => {
            let views = p.lane_views(now);
            let mut wires: std::collections::BTreeMap<(String, String), Wire> = Default::default();
            for v in views.iter().filter(|v| v.upstream_wire.is_some()) {
                let k = (v.provider.clone(), v.model.clone());
                if let std::collections::btree_map::Entry::Vacant(e) = wires.entry(k) {
                    if let Some(w) = p.lane_wire(&v.provider, &v.model, now) {
                        e.insert(w);
                    }
                }
            }
            (views, wires)
        }
        Err(_) => return json!([]),
    };

    let mut offers: std::collections::BTreeMap<(String, String), LaneOffer> = Default::default();
    let mut wire_conflicts = 0usize;
    for v in views {
        // An account the operator has not switched on for selling is not on
        // the market at all — not even as a paused lane.
        if !v.sell_enabled {
            continue;
        }
        let key = (v.provider.clone(), v.model.clone());
        // A lane carries one dialect. An endpoint of this device that serves
        // the same model in another one is left out rather than added in: its
        // capacity would be declared under a dialect it cannot answer, and the
        // gateway would build a body it can only refuse. It stays sellable for
        // every model the winning dialect does not already cover.
        if let (Some(w), Some(own)) = (lane_wires.get(&key), v.upstream_wire) {
            if own != *w {
                wire_conflicts += 1;
                continue;
            }
        }
        let offer = offers.entry(key).or_default();
        offer.credentials.insert(credential_mark(&v.provider, &v.account_id, &v.upstream_base));
        if v.status == "selling" {
            // Each account's headroom is its own (spec §4): summing here never
            // lets one account borrow another's, because `rebuild_pool`
            // computed each figure from that account's own daily cap.
            //
            // An account the operator has not capped declares the same flat,
            // large window a metered key does: there is no local number that
            // says how much of a subscription is left, and inventing one is
            // exactly what used to take live lanes off the market.
            offer.window_remaining = offer.window_remaining.saturating_add(match v.quota_remaining {
                asale_client_core::pool::UNCAPPED => CUSTOM_WINDOW_TOKENS as i64,
                q => q.min(i64::MAX as u64) as i64,
            });
            // The cheapest of them is what this device is asking: a buyer that
            // meets it gets served by that account, whatever the others want.
            offer.ask_ratio = Some(offer.ask_ratio.map_or(v.min_ratio, |a| a.min(v.min_ratio)));
            // The seller's own ceiling, not a constant: the gateway declines to
            // send an account more than this many tasks at once, which is what
            // makes the setting binding rather than advisory — enforcing it only
            // here would mean refusing work already routed to us, and the market
            // charges that to the lane's reputation.
            offer.concurrency_free += v.concurrency_max as i32;
        } else {
            let reason = v.paused_reason.clone().unwrap_or_else(|| match v.status.as_str() {
                "cooldown" => "cooldown".into(),
                "expired" => "auth".into(),
                other => other.into(), // exhausted → quota, below
            });
            let reason = if reason == "exhausted" { "quota".into() } else { reason };
            let resume_at = v.resume_at.max(v.cooldown_until.unwrap_or(0));
            // Prefer the reason that comes back soonest, so a lane with one
            // broken account and one merely cooling reads as "back in 30s"
            // rather than "needs attention".
            let better = match &offer.pause {
                None => true,
                Some((_, prev)) => soonest(resume_at, *prev),
            };
            if better {
                offer.pause = Some((reason, resume_at));
            }
        }
    }

    // Built as the shared `SupplyItem`, not as a hand-spelled `json!` object:
    // this is the frame the gateway deserializes into that very struct, so the
    // field names are the server's to choose and ours to compile against.
    let items: Vec<SupplyItem> = offers
        .into_iter()
        .filter_map(|((provider, model), o)| {
            // A provider this build does not know is not declarable — the
            // gateway would reject the whole frame on the unknown variant.
            let wire = lane_wires.get(&(provider.clone(), model.clone())).copied();
            let provider = Provider::from_str_opt(&provider)?;
            let mut item = SupplyItem::offered(
                &model,
                provider,
                o.window_remaining,
                price_min,
                "",
                o.concurrency_free,
            );
            // Declared only where it is the lane's own answer; leaving it empty
            // for everyone else is what keeps the gateway building a
            // subscription's body from the vendor it belongs to.
            if let Some(w) = wire {
                item = item.speaking(w);
            }
            // Declared whenever an account is serving this lane, so the market
            // can price an idle minute at the best ask instead of at its floor.
            if let Some(ask) = o.ask_ratio {
                item = item.asking(ask as i32);
            }
            item.credential_fp = lane_credential_fp(&o.credentials);
            Some(if o.window_remaining > 0 {
                item
            } else {
                let (reason, resume_at) = o.pause.unwrap_or_else(|| ("quota".to_string(), 0));
                item.paused(&reason, resume_at)
            })
        })
        .collect();
    if wire_conflicts > 0 {
        tracing::warn!(
            "{wire_conflicts} lane(s) left out: another endpoint on this device already offers \
             those models in a different protocol, and a lane can only be sold in one"
        );
    }
    json!(items)
}

/// A stable, opaque mark for one account's credential.
///
/// # What goes in
///
/// What identifies *which subscription* is behind the lane, and nothing else:
/// the provider, the account key, and — for the one provider whose host is the
/// operator's own — where its requests go. Repointing a custom endpoint at a
/// different upstream is exactly the change this exists to notice, and it is
/// invisible in the account id alone.
///
/// Not the token. The token rotates on its own schedule — refreshed hourly on
/// some providers — and a mark that moved with it would demand a fresh
/// verification every hour for no reason at all.
///
/// # Why it is salted
///
/// The mark leaves this machine, so an unsalted hash of an email address is an
/// email address: the set of possible inputs is small enough to enumerate, and
/// the same seller on two devices would hash identically, making the two
/// linkable by anyone who saw both. The salt is per install and never leaves,
/// which keeps the value stable where it needs to be — this lane, this device,
/// over time — and meaningless anywhere else.
fn credential_mark(provider: &str, account_id: &str, upstream_base: &Option<String>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(credential_salt().as_bytes());
    h.update(b"|");
    h.update(provider.as_bytes());
    h.update(b"|");
    h.update(account_id.as_bytes());
    h.update(b"|");
    h.update(upstream_base.as_deref().unwrap_or("").as_bytes());
    hex16(&h.finalize())
}

/// One lane's mark, over every account that contributes to it.
///
/// A lane can be served by several accounts, and the question the gateway asks
/// is "is this the same set of credentials I verified", not "is this the same
/// one". Adding a second subscription to a lane changes what a buyer may be
/// served by, so it should cost a re-verification; the sorted set makes that
/// the only thing that changes it.
fn lane_credential_fp(marks: &std::collections::BTreeSet<String>) -> String {
    use sha2::{Digest, Sha256};
    if marks.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    for m in marks {
        h.update(m.as_bytes());
        h.update(b"|");
    }
    hex16(&h.finalize())
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Per-install salt for [`credential_mark`], generated once and kept locally.
///
/// Read through a `OnceLock` because it is consulted for every account on
/// every declaration — once a minute, forever — and the file behind it never
/// changes.
fn credential_salt() -> &'static str {
    static SALT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SALT.get_or_init(|| {
        let path = std::path::PathBuf::from(crate::state::data_dir()).join("credential-salt");
        if let Ok(s) = std::fs::read_to_string(&path) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
        let fresh = uuid::Uuid::new_v4().simple().to_string();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // A failed write is survivable and must not be fatal: the salt is
        // regenerated next start, every mark changes, and the platform asks
        // for one extra verification per lane. Refusing to publish over it
        // would be the worse trade.
        if let Err(e) = std::fs::write(&path, &fresh) {
            tracing::warn!("could not persist the credential salt ({e}); lanes will re-verify once");
        }
        fresh
    })
}

/// Whether `a` is a nearer (and known) return time than `b`. 0 means "needs the
/// operator", which is the furthest away there is.
fn soonest(a: i64, b: i64) -> bool {
    match (a, b) {
        (0, _) => false,
        (_, 0) => true,
        (a, b) => a < b,
    }
}

/// Resolve the plan label for a tool row: prefer the settings entry written at
/// OAuth/import time, fall back to the tools column.
async fn resolve_plan(store: &LocalStore, tool: &asale_client_core::store::ToolRow) -> Option<String> {
    if let Ok(Some(p)) = store.get_setting(&format!("plan:{}:{}", tool.provider, tool.account_id)).await {
        if !p.is_empty() {
            return Some(p);
        }
    }
    tool.plan.clone()
}

// ── TokenProvider: pool-selected account, secret-store lookup at injection ──

/// A lane state change worth acting on outside the executor's hot path.
///
/// `report` runs on the task's thread holding the pool lock, so it must not
/// touch SQLite or the socket. It posts one of these instead; `spawn_lane_loop`
/// does the slow half — persist the pause, then re-declare supply so the
/// gateway stops sending work this device has just decided it cannot do.
#[derive(Debug, Clone)]
pub struct LaneEvent {
    pub provider: String,
    pub account_id: String,
    pub model: String,
    /// None = the lane merely changed (cooldown, quota decay): re-declare only.
    pub paused: Option<String>,
    pub last_error: String,
}

pub type LaneSender = tokio::sync::mpsc::UnboundedSender<LaneEvent>;

/// Pool-backed token provider (spec §4). `acquire` picks an account whose lane
/// for the requested model is serving, reads its token from the encrypted
/// secret store at injection time, and `report` feeds the outcome back into the
/// lane's recovery ladder (§4.5).
pub struct PoolTokens {
    pub pool: Arc<StdMutex<AccountPool>>,
    pub lanes: Option<LaneSender>,
}

impl PoolTokens {
    fn emit(&self, provider: &str, account_id: &str, model: &str, paused: Option<PauseReason>, last_error: &str) {
        let Some(tx) = &self.lanes else { return };
        let _ = tx.send(LaneEvent {
            provider: provider.to_string(),
            account_id: account_id.to_string(),
            model: model.to_string(),
            paused: paused.map(|r| r.as_str().to_string()),
            last_error: last_error.to_string(),
        });
    }
}

impl TokenProvider for PoolTokens {
    fn token_for(&self, provider: &str) -> Option<String> {
        self.acquire(provider, "").map(|l| l.token)
    }

    fn session_for(&self, account_id: &str) -> Option<String> {
        crate::session::claude_session_for(account_id)
    }

    fn has_alternate(&self, provider: &str, model: &str, except: &str) -> bool {
        self.pool.lock().is_ok_and(|pool| pool.alternate_available(provider, model, except, now_secs()))
    }

    fn acquire(&self, provider: &str, model: &str) -> Option<LeasedToken> {
        // Sale traffic may only ever be served by an account the user switched
        // on for selling, on a lane that is not cooling or paused —
        // `pick_for_sale`, not `pick`.
        //
        // And, for a provider whose accounts each hold their own endpoint, only
        // by one speaking the dialect this lane was declared in: the body in
        // hand was built for that dialect, and the same rule chose it here as
        // chose it in the declaration.
        let picked = {
            let mut pool = self.pool.lock().ok()?;
            let now = now_secs();
            let wire = pool.lane_wire(provider, model, now);
            pool.pick_for_sale(provider, model, wire, now)?
        };
        match keychain::get(&picked.keychain_ref).ok().flatten() {
            Some(token) => Some(LeasedToken {
                token,
                account_id: picked.account_id,
                session_id: None,
                upstream_account_id: picked.upstream_account_id,
                upstream_base: picked.upstream_base,
                upstream_wire: picked.upstream_wire,
                upstream_model: picked.upstream_model,
            }),
            None => {
                // Keychain entry vanished — release the lease and flag the account.
                let paused = self.pool.lock().ok().and_then(|mut pool| {
                    pool.on_error(provider, &picked.account_id, model, UpstreamErrorKind::AuthFailed, "missing credential", now_secs())
                });
                self.emit(provider, &picked.account_id, model, paused, "missing credential");
                None
            }
        }
    }

    fn saturated(&self, provider: &str, model: &str) -> bool {
        let Ok(pool) = self.pool.lock() else { return false };
        let now = now_secs();
        let wire = pool.lane_wire(provider, model, now);
        pool.lane_saturated(provider, model, wire, now)
    }

    fn report(&self, provider: &str, account_id: &str, model: &str, outcome: TaskOutcome) {
        if account_id.is_empty() {
            return;
        }
        let now = now_secs();
        let (paused, detail) = {
            let Ok(mut pool) = self.pool.lock() else { return };
            match outcome {
                TaskOutcome::Success { tokens_used } => {
                    pool.on_success(provider, account_id, model, tokens_used);
                    (None, String::new())
                }
                TaskOutcome::RateLimited { reset_at } => {
                    let d = "upstream rate limit (429)";
                    (pool.on_error(provider, account_id, model, UpstreamErrorKind::RateLimited { reset_at }, d, now), d.to_string())
                }
                TaskOutcome::ServerError => {
                    let d = "upstream error (5xx/transport)";
                    (pool.on_error(provider, account_id, model, UpstreamErrorKind::ServerError, d, now), d.to_string())
                }
                TaskOutcome::AuthFailed => {
                    let d = "authentication failed (401/403)";
                    (pool.on_error(provider, account_id, model, UpstreamErrorKind::AuthFailed, d, now), d.to_string())
                }
                TaskOutcome::Unsupported => {
                    let d = "upstream does not serve this model";
                    (pool.on_error(provider, account_id, model, UpstreamErrorKind::Unsupported, d, now), d.to_string())
                }
                // The pool treats it as the rate limit it is; the operator is
                // told what it actually is, because waiting this one out does
                // nothing — the allowance only comes back if they top it up.
                // Which top-up depends on whose allowance ran out: a Claude
                // subscription has one page, a resold API key has whatever its
                // vendor's is, and naming Anthropic's to the operator of an
                // empty OpenRouter key sends them to the wrong wallet.
                TaskOutcome::QuotaExhausted { reset_at } => {
                    let d = if asale_protocol::ids::is_claude_family(provider) {
                        "extra usage exhausted — top up at claude.ai/settings/usage"
                    } else {
                        "upstream credit exhausted — top up this account with its provider"
                    };
                    (pool.on_error(provider, account_id, model, UpstreamErrorKind::RateLimited { reset_at }, d, now), d.to_string())
                }
                TaskOutcome::Blocked => {
                    let d = "upstream refused this machine (403 — region block or network filter)";
                    (pool.on_error(provider, account_id, model, UpstreamErrorKind::Blocked, d, now), d.to_string())
                }
            }
        };
        // A success is worth reporting too: it may have cleared a cooldown, and
        // the market should hear about restored capacity as promptly as it
        // hears about lost capacity.
        self.emit(provider, account_id, model, paused, &detail);
    }
}

/// Receives `control` frames the gateway sends when *its* backstop takes one of
/// this device's lanes out (spec §4.5). The client usually gets there first;
/// this is what keeps the two views from diverging when it does not.
pub struct GatewayLaneControl {
    pub pool: Arc<StdMutex<AccountPool>>,
    pub lanes: Option<LaneSender>,
}

impl asale_client_core::LaneControl for GatewayLaneControl {
    fn on_lane_pause(&self, model: &str, reason: &str, requires_user: bool) {
        if model.is_empty() {
            return;
        }
        // Which account served it is not in the frame — the gateway only knows
        // the device — so every account that sells this model is paused. They
        // share one device and one operator; a resume clears them together.
        //
        // Prefer the gateway's own reason: it distinguishes cases the client
        // cannot infer (a model the platform stopped trading is not a rate
        // limit), and `requires_user` alone would collapse them all into two.
        let cause = PauseReason::parse(reason)
            .unwrap_or(if requires_user { PauseReason::Breaker } else { PauseReason::RateLimit });
        let mut touched: Vec<(String, String)> = Vec::new();
        if let Ok(mut pool) = self.pool.lock() {
            for v in pool.lane_views(now_secs()) {
                if v.model == model && v.sell_enabled {
                    touched.push((v.provider, v.account_id));
                }
            }
            for (provider, account_id) in &touched {
                pool.pause_lane(provider, account_id, model, cause, 0);
            }
        }
        for (provider, account_id) in touched {
            if let Some(tx) = &self.lanes {
                let _ = tx.send(LaneEvent {
                    provider,
                    account_id,
                    model: model.to_string(),
                    paused: Some(cause.as_str().to_string()),
                    last_error: format!("gateway: {reason}"),
                });
            }
        }
    }
}

/// Rebuild the pool's account set from the store (called at startup, after any
/// account change, and periodically so quota estimates stay fresh). Preserves
/// live cooldown state for accounts that persist.
pub async fn rebuild_pool(store: &LocalStore, pool: &StdMutex<AccountPool>) {
    let tools = match store.list_tools().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("pool rebuild: list_tools failed: {e}");
            return;
        }
    };
    let catalog = load_catalog(store).await;
    // An account synced out of a local CLI's own directory stops being this
    // device's to sell while that CLI is buying through the market. Dropped from
    // the pool rather than merely switched off, because the pool is also what
    // `list_accounts` renders: the sell page must not show an account it is not
    // offering. asale's own logins (`origin = oauth`) and pasted keys are
    // unaffected — they are not the local CLI's credential.
    let buying = crate::tool_config::buying_set(
        store,
        tools.iter().filter(|t| t.origin.as_deref() == Some("import")).map(|t| t.provider.as_str()),
    )
    .await;
    let mut fresh = Vec::new();
    // Lanes a *model-scoped* upstream window has spent — Opus's own weekly cap
    // on top of the account-wide ones. Collected here and applied after
    // `set_accounts`, which is the call that decides which lanes exist.
    let mut scoped_blocks: Vec<(String, String, String, i64)> = Vec::new();
    for tool in &tools {
        if tool.origin.as_deref() == Some("import") && buying.contains(&tool.provider) {
            continue;
        }
        let plan = resolve_plan(store, tool).await;
        let expires_at: Option<i64> = store
            .get_setting(&exp_key(&tool.provider, &tool.account_id))
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
        let used_today = store
            .served_tokens_today_for_account(&tool.provider, &tool.account_id)
            .await
            .unwrap_or(0);
        // What the provider itself says about this account's windows, if a
        // recent reading is banked. Read for two things only: the *model-scoped*
        // windows below (a weekly Opus cap the vendor names, which takes that
        // one lane off and leaves the rest selling), and the Sell page's
        // utilisation bar.
        //
        // Deliberately *not* turned into a token headroom any more. It used to
        // be — `plan_cap × headroom − sales since the reading` — and a guessed
        // cap an order of magnitude too small meant four minutes of real agent
        // traffic zeroed it, pulled every lane of the account off the market,
        // and left the only seller of a model unbuyable until the next five
        // minute poll put it back. The subscription's real limit is the
        // subscription's to state: a 429 cools the whole account until the reset
        // it names (`AccountPool::on_error`).
        let gate = crate::commands::usage::account_quota_gate(store, &tool.provider, &tool.account_id, now_secs()).await;
        // Two ways an account stops, and neither is an estimate:
        //
        //   * the vendor's own reading says an account-wide window is *fully*
        //     spent — its verdict, in its own units, with the instant it comes
        //     back. Selling into that only burns buyer requests and this
        //     device's reputation on 429s we were told about in advance;
        //   * the operator's own daily cap, which they typed in.
        //
        // Everything in between — "77% of a guessed 4.4M, minus what we sold
        // since the reading" — is gone.
        let vendor_spent = gate.as_ref().is_some_and(|(g, _)| g.exhausted());
        let quota = match (vendor_spent, tool.sell_daily_limit) {
            (true, _) => 0,
            (_, lim) if lim > 0 => (lim as u64).saturating_sub(used_today),
            _ => asale_client_core::pool::UNCAPPED,
        };
        // Codex is the one family whose upstream serves a narrower set than the
        // catalog lists, and the set belongs to the account rather than to the
        // provider — so it is resolved per account, here.
        // The endpoint's own `/models`, indexed by market id. Only a custom
        // account has one; the aliases ride onto the account below so the
        // executor can put the endpoint's own spelling back into the body.
        let listing = match tool.provider.as_str() {
            CUSTOM_PROVIDER => Some(custom_endpoint_models(store, tool).await),
            // Alibaba serves a different set from the one OpenRouter prices
            // under `qwen/*`, so its own `/models` decides — see
            // `vendor_endpoint_models` for what goes wrong without this.
            p if p == Provider::Qwen.as_str() => {
                Some(vendor_endpoint_models(store, tool, Provider::Qwen).await)
            }
            _ => None,
        };
        // An OpenAI-schema endpoint with no `/responses` route cannot serve the
        // responses-only models at all: every request that carries a tool comes
        // back 400, which costs a buyer a turn and this device its reputation
        // for a lane that could never have worked. With the route,
        // `AccountRuntime::wire_for` offers them there instead. Read after the
        // listing, which is what probes and records the answer.
        let lite_unsellable = tool.provider == CUSTOM_PROVIDER
            && custom_wire(store, &tool.account_id).await == Wire::Openai
            && !custom_has_responses(store, &tool.account_id).await;
        let entitled = match tool.provider.as_str() {
            "codex" => Some(codex_entitlement(store, tool).await),
            // Same contract as Codex's entitlement: an empty answer means
            // advertise nothing, because a model the endpoint will refuse costs a
            // consumer a failed turn and this device its reputation.
            p if Provider::from_str_opt(p).is_some_and(asale_protocol::providers::resells_other_vendors) => Some(offerable(
                listing.as_ref().map(|l| l.market_ids()).unwrap_or_default(),
                lite_unsellable,
            )),
            _ => None,
        };
        let mut a = AccountRuntime::new(&tool.provider, &tool.account_id, &tool.keychain_ref)
            .with_models(sellable_models(&catalog, &tool.provider, entitled.as_deref()));
        a.upstream_account_id = store
            .get_setting(&upstream_acct_key(&tool.provider, &tool.account_id))
            .await
            .ok()
            .flatten()
            .filter(|s| !s.is_empty());
        a.plan = plan;
        a.quota_remaining = quota;
        // Whichever of the two stopped it says when it ends: the vendor names
        // the instant its window resets, the operator's cap rolls over with the
        // metering day.
        a.quota_reset_at = match (quota == 0, vendor_spent) {
            (false, _) => None,
            (true, true) => gate.as_ref().and_then(|(g, _)| g.reset_at),
            (true, false) => Some(crate::commands::day_start_ts() + 86_400),
        };
        a.expires_at = expires_at;
        a.sell_enabled = tool.sell_enabled;
        a.origin = tool.origin.clone();
        a.used_today = used_today;
        a.sell_daily_limit = tool.sell_daily_limit;
        a.sell_models = tool.sell_models.clone();
        a.sell_min_ratio = tool.sell_min_ratio;
        a.sell_max_ratio = tool.sell_max_ratio;
        a.concurrency_max = tool.sell_concurrency as u32;
        // Only a custom account has one, and without it the executor would send
        // its traffic to the gateway's placeholder host — which is exactly the
        // failure the placeholder is chosen to make loud.
        a.upstream_base = store
            .get_setting(&custom_base_key(&tool.account_id))
            .await
            .ok()
            .flatten()
            .filter(|s| !s.is_empty());
        // Read for a custom account only: it is what turns that base into a
        // full URL and decides the header its key is sent in.
        a.upstream_wire = match a.upstream_base.is_some() {
            true => Some(custom_wire(store, &tool.account_id).await),
            false => None,
        };
        // Same read, same reason: it is what lets one endpoint offer the
        // responses-only models on the route that can serve them.
        a.upstream_responses = custom_has_responses(store, &tool.account_id).await;
        // Empty for every other provider: their model ids are the market's
        // already, so there is nothing to translate on the way out.
        a.model_aliases = listing.map(|l| l.aliases).unwrap_or_default();
        // A spent model-scoped window takes its own models off the market and
        // leaves the rest of the subscription selling. Resolved against the
        // lanes this account actually has, so a scope naming a model it does
        // not sell costs nothing.
        if let Some((g, _)) = &gate {
            for model in a.lanes.keys() {
                if let Some(b) = g.scope_block(model) {
                    // A block with no reset instant still gets one: a lane
                    // paused with `resume_at = 0` waits for an operator, and
                    // nobody should have to press a button to end a window the
                    // vendor will roll over on its own. An hour is short enough
                    // to re-ask soon and long enough not to hammer the upstream.
                    let resume_at = b.reset_at.unwrap_or_else(|| now_secs() + 3600);
                    scoped_blocks.push((
                        tool.provider.clone(),
                        tool.account_id.clone(),
                        model.clone(),
                        resume_at,
                    ));
                }
            }
        }
        fresh.push(a);
    }
    // Pauses that need a person outlive the process: without this a restart
    // would put a lane the operator was asked to fix straight back on the
    // market, which is exactly the flapping the breaker exists to stop.
    let persisted = store.list_lane_pauses().await.unwrap_or_default();
    // The cached market prices, judged against each account's band *after*
    // `set_accounts` — that is the call which restores the hysteresis state the
    // verdict is built on, so applying the band before it would forget every
    // pending re-entry on each rebuild (which is once a minute, and on every
    // account edit).
    let ratios = catalog.map(|c| c.ratios).unwrap_or_default();
    if let Ok(mut p) = pool.lock() {
        p.set_accounts(fresh);
        for (provider, account_id, model, reason, _err) in persisted {
            if let Some(r) = PauseReason::parse(&reason) {
                p.pause_lane(&provider, &account_id, &model, r, 0);
            }
        }
        // Not persisted, unlike the pauses above: this one carries the vendor's
        // own reset instant, so it clears itself the moment that passes and a
        // rebuild that no longer sees the block simply stops re-applying it.
        for (provider, account_id, model, resume_at) in scoped_blocks {
            p.pause_lane(&provider, &account_id, &model, PauseReason::Quota, resume_at);
        }
        p.apply_prices(&ratios);
    }
}

/// Drain lane state changes: persist the ones a person has to clear, then push
/// the new offering to the gateway (spec §4.5 recovery).
///
/// The nudge is the whole point. A lane this device has paused but never
/// re-declared stays in the gateway's rotation for up to a full refresh
/// interval, and every request routed there in the meantime fails, costs the
/// consumer a failover and this device a reputation hit.
pub fn spawn_lane_loop(
    store: Arc<LocalStore>,
    publisher: Arc<tokio::sync::RwLock<Option<PublisherHandle>>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LaneEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev.paused.as_deref().and_then(PauseReason::parse) {
                Some(r) if r.requires_user() => {
                    tracing::warn!(
                        provider = %ev.provider, account = %ev.account_id, model = %ev.model,
                        "lane paused ({}) — waiting for the operator: {}", r.as_str(), ev.last_error
                    );
                    let _ = store
                        .set_lane_pause(&ev.provider, &ev.account_id, &ev.model, r.as_str(), &ev.last_error, now_secs())
                        .await;
                }
                Some(r) => tracing::info!(
                    provider = %ev.provider, model = %ev.model,
                    "lane paused ({}) — resumes on its own", r.as_str()
                ),
                None => {}
            }
            if let Some(h) = publisher.read().await.as_ref() {
                h.nudge();
            }
        }
    })
}

/// Wake up exactly when a lane becomes serviceable again and re-declare.
///
/// The clocks that bring capacity back on their own are a lane's own
/// `resume_at` (a rate limit's reset, a cooldown rung), the UTC day rollover
/// that clears the per-account daily sell caps, and the platform listing a
/// model it was not trading before. The first two are known instants, so
/// waiting for the 60s periodic re-declaration to stumble over them just
/// leaves capacity idle and the seller unpaid for up to a minute; the third is
/// polled on `CATALOG_TTL`.
///
/// The fourth clock is the market's own: a model's price moving in or out of
/// an account's price band changes what this device is offering without
/// anything local having happened at all, so the prices are polled on
/// `PRICE_TTL` and a changed verdict is pushed the same way a cooldown is.
pub fn spawn_recovery_loop(
    store: Arc<LocalStore>,
    pool: Arc<StdMutex<AccountPool>>,
    publisher: Arc<tokio::sync::RwLock<Option<PublisherHandle>>>,
    server_api_base: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = now_secs();
            let lane_at = pool.lock().ok().and_then(|p| p.next_auto_resume(now));
            let midnight = next_utc_midnight(now);
            // Cap the sleep so a lane that starts cooling *after* we computed
            // this still gets a timely wake-up.
            let wake = lane_at.unwrap_or(i64::MAX).min(midnight).min(now + 60);
            let delay = (wake - now).max(1) as u64;
            tokio::time::sleep(Duration::from_secs(delay)).await;

            // A model the platform started (or stopped) trading changes which
            // lanes exist at all, so this has to land before the rebuild.
            let mut catalog_changed = false;
            if catalog_is_stale(&store).await {
                match refresh_sellable_catalog(&store, &server_api_base).await {
                    Ok(changed) => catalog_changed = changed,
                    Err(e) => tracing::warn!("sellable catalog refresh failed: {e}"),
                }
            }

            // The market repriced: a lane can cross its account's band in
            // either direction without anything on this device changing.
            if prices_are_stale(&store).await {
                if let Err(e) = refresh_market_prices(&store, &server_api_base).await {
                    tracing::warn!("market price refresh failed: {e}");
                }
            }

            let withheld_before = withheld_lanes(&pool);
            // Recompute quota/daily-cap headroom, then let the pool decide
            // whether anything actually became serviceable.
            rebuild_pool(&store, &pool).await;
            let withheld_after = withheld_lanes(&pool);
            let ready = pool
                .lock()
                .map(|p| p.lane_views(now_secs()).iter().any(|v| v.status == "selling"))
                .unwrap_or(false);
            if withheld_before != withheld_after {
                tracing::info!(
                    withheld = withheld_after.len(),
                    "price band verdict changed — re-declaring what this device is offering"
                );
            }
            // A catalog change must be pushed even when nothing is serving:
            // withdrawing a model the platform dropped is exactly as urgent as
            // announcing one it added. So is a lane the price band has just
            // taken out — leaving it advertised is how a device ends up selling
            // at a price its operator refused.
            if ready || catalog_changed || withheld_before != withheld_after {
                if let Some(h) = publisher.read().await.as_ref() {
                    h.nudge();
                }
            }
        }
    })
}

/// The lanes currently held back on price, as a sorted set of
/// `provider:account:model` — compared across a rebuild to tell whether the
/// market moved this device's offering.
fn withheld_lanes(pool: &StdMutex<AccountPool>) -> std::collections::BTreeSet<String> {
    let Ok(p) = pool.lock() else { return Default::default() };
    p.lane_views(now_secs())
        .into_iter()
        .filter(|v| v.status == "withheld")
        .map(|v| format!("{}:{}:{}", v.provider, v.account_id, v.model))
        .collect()
}

/// Start of the next UTC day, in unix seconds.
fn next_utc_midnight(now: i64) -> i64 {
    const DAY: i64 = 86_400;
    now - now.rem_euclid(DAY) + DAY
}

// ── RecordSink: per-task metering into provider_records ─────────────────────

struct StoreRecordSink {
    store: Arc<LocalStore>,
}

#[async_trait]
impl RecordSink for StoreRecordSink {
    async fn record(&self, task_id: &str, provider: &str, account_id: &str, model: &str, usage: &Usage, status: &str) {
        let _ = self
            .store
            .insert_provider_record(
                task_id,
                provider,
                account_id,
                model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                status,
            )
            .await;
    }

    /// Codex states its remaining quota on every accepted response, and xAI
    /// volunteers an `x-ratelimit-*` block on its own. Serving a task is
    /// therefore a free reading of the seller's own subscription, and banking it
    /// here is what keeps the Limits page current without paying for a probe —
    /// for xAI, whose subscription answers no usage endpoint at all, it is the
    /// only reading there is.
    async fn observe_quota(&self, provider: &str, account_id: &str, headers: &std::collections::BTreeMap<String, String>) {
        crate::commands::usage::record_quota_headers(&self.store, provider, account_id, headers).await;
    }
}

// ── Public entry points (called by commands.rs) ─────────────────────────────

/// Start the publisher session. Returns the handle whose state the UI polls.
pub async fn start(state: &AppState) -> anyhow::Result<PublisherHandle> {
    // Fail fast if not signed in — the device can't register otherwise.
    if keychain::get("access_token")?.is_none() {
        anyhow::bail!("sign in before publishing");
    }
    let lane_tx = state.lane_tx.clone();
    let cfg_src: Arc<dyn ConfigSource> = Arc::new(RestConfigSource {
        server_api_base: state.cfg.server_api_base.clone(),
        gateway_ws_url: state.cfg.gateway_ws_url.clone(),
        device_id: state.device_id.clone(),
        device_pubkey: state.identity.public_key_b64(),
    });

    // Declare against what the platform trades *now*, not what it traded when
    // the daemon last polled — going on the market is exactly the moment a
    // stale catalog would cost the seller a lane.
    if catalog_is_stale(&state.store).await {
        if let Err(e) = refresh_sellable_catalog(&state.store, &state.cfg.server_api_base).await {
            tracing::warn!("sellable catalog refresh failed, using the cached set: {e}");
        }
    }
    // And against what the market pays *now*, for the same reason: going on the
    // market judging the price bands against a ten-minute-old price is how a
    // device sells a window at a price its operator had ruled out.
    if prices_are_stale(&state.store).await {
        if let Err(e) = refresh_market_prices(&state.store, &state.cfg.server_api_base).await {
            tracing::warn!("market price refresh failed, using the cached prices: {e}");
        }
    }
    // Make sure the pool reflects the current account set before serving.
    rebuild_pool(&state.store, &state.pool).await;
    // Resolved before the socket opens, and fatal when absent: a build with no
    // pinned gateway key cannot tell an authorized dispatch from a forged one,
    // and the cost of guessing wrong is the user's own subscription.
    let deps = PublisherDeps::with_pinned_quota_key(
        state.identity.clone(),
        Arc::new(PoolTokens {
            pool: state.pool.clone(),
            lanes: Some(lane_tx.clone()),
        }),
        Arc::new(PoolSupplySource { store: state.store.clone(), pool: state.pool.clone() }),
        Some(Arc::new(StoreRecordSink { store: state.store.clone() })),
        Some(Arc::new(GatewayLaneControl { pool: state.pool.clone(), lanes: Some(lane_tx) })),
    )?;

    Ok(spawn_publisher(cfg_src, deps))
}

/// The token-refresh loop (spec §3.4): every minute, renew any subscription
/// access token nearing expiry using its refresh token, persisting the result.
/// Also rebuilds the account pool so quota estimates and refreshed expiries
/// propagate into selection (spec §4).
pub fn spawn_refresh_loop(store: Arc<LocalStore>, pool: Arc<StdMutex<AccountPool>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            if let Err(e) = refresh_due_tokens(&store, &pool).await {
                tracing::warn!("token refresh cycle error: {e}");
            }
            rebuild_pool(&store, &pool).await;
        }
    })
}

async fn refresh_due_tokens(store: &LocalStore, pool: &Arc<StdMutex<AccountPool>>) -> anyhow::Result<()> {
    let tools = store.list_tools().await?;
    let now = now_secs();
    for tool in tools {
        let adapter = match adapter_for(&tool.provider) {
            Some(a) => a,
            None => continue,
        };
        let exp: Option<i64> = store
            .get_setting(&exp_key(&tool.provider, &tool.account_id))
            .await?
            .and_then(|s| s.parse().ok());
        if !discovery::needs_refresh(exp, adapter.refresh_lead(), now) {
            continue;
        }
        let refresh_token = match keychain::get(&keychain::refresh_ref(&tool.provider, &tool.account_id))? {
            Some(r) if !r.is_empty() => r,
            _ => continue,
        };
        match adapter.refresh(&refresh_token).await {
            Ok(t) => {
                persist_refresh(store, &tool.provider, &tool.account_id, &t).await?;
                write_back_shared_credential(&tool, &t, now).await;
                // The vendor just handed us a working token for this account,
                // which is a direct answer to whatever `auth` pause the old one
                // earned. Cleared on disk before the pool, because the rebuild
                // at the end of this tick re-applies the persisted rows.
                let cleared = store
                    .clear_lane_pause_reason(&tool.provider, &tool.account_id, "auth")
                    .await
                    .unwrap_or_default();
                let in_pool = pool
                    .lock()
                    .map(|mut p| p.clear_auth_failure(&tool.provider, &tool.account_id))
                    .unwrap_or_default();
                if !cleared.is_empty() || !in_pool.is_empty() {
                    tracing::info!(
                        provider = %tool.provider, account = %tool.account_id,
                        "token refresh succeeded — clearing the account's authentication pause"
                    );
                }
            }
            Err(e) => tracing::warn!(provider = %tool.provider, "refresh failed: {e}"),
        }
    }
    Ok(())
}

/// Put a refreshed token back into the CLI files an imported account came from.
///
/// An `origin = "import"` account is the locally installed CLI's own login, and
/// both Anthropic and OpenAI rotate the refresh token on redemption: the copy in
/// `~/.claude/.credentials.json` dies the instant asale refreshes. Storing the
/// replacement only in asale's own keychain therefore logs the user out of their
/// own Claude Code some hours after they start the app, with nothing connecting
/// the two events — so the new token goes back where the CLI reads it.
///
/// Best-effort by design: every failure here leaves asale itself working (it has
/// the token) and is reported rather than propagated, because a credential file
/// asale does not recognize is one it must not overwrite with a guess.
///
/// Accounts asale logged in itself (`origin = "oauth"`) share nothing and are
/// skipped — writing into a CLI's directory then would be asale reaching into
/// files that are not its business.
async fn write_back_shared_credential(
    tool: &asale_client_core::store::ToolRow,
    t: &RefreshedToken,
    now: i64,
) {
    if tool.origin.as_deref() != Some("import") {
        return;
    }
    // `sources` lists every store this one account was found in; a
    // `keychain:<service>` entry is not a path we can write.
    let paths: Vec<String> = tool
        .sources
        .iter()
        .chain(tool.source.iter())
        .filter(|s| !s.starts_with("keychain:") && *s != "oauth")
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if paths.is_empty() {
        tracing::warn!(
            provider = %tool.provider, account = %tool.account_id,
            "refreshed a credential shared with the local CLI but found no file to write it back to — \
             that CLI will be signed out; import it again or sign in to it once more"
        );
        return;
    }

    let (provider, account) = (tool.provider.clone(), tool.account_id.clone());
    let (access, refresh, expires_at) = (t.access_token.clone(), t.refresh_token.clone(), t.expires_at);
    let _ = tokio::task::spawn_blocking(move || {
        for path in paths {
            let cred = asale_client_core::cli_import::RefreshedCred {
                access_token: &access,
                refresh_token: refresh.as_deref(),
                expires_at,
                now_secs: now,
            };
            let written = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|raw| asale_client_core::cli_import::patch_cli_credentials(&provider, &raw, cred))
                .and_then(|body| crate::tool_config::write_atomic(std::path::Path::new(&path), &body));
            match written {
                Ok(()) => tracing::info!(provider = %provider, account = %account, %path, "wrote the refreshed token back to the CLI"),
                Err(e) => tracing::warn!(
                    provider = %provider, account = %account, %path,
                    "could not write the refreshed token back; that CLI will be signed out: {e}"
                ),
            }
        }
    })
    .await;
}

/// Persist a refreshed token set into the secret store + local store (spec §3.4).
pub async fn persist_refresh(
    store: &LocalStore,
    provider: &str,
    account_id: &str,
    t: &RefreshedToken,
) -> anyhow::Result<()> {
    keychain::set(&keychain::token_ref(provider, account_id), &t.access_token)?;
    if let Some(r) = &t.refresh_token {
        keychain::set(&keychain::refresh_ref(provider, account_id), r)?;
    }
    if let Some(exp) = t.expires_at {
        store.set_setting(&exp_key(provider, account_id), &exp.to_string()).await?;
    }
    Ok(())
}

pub fn exp_key(provider: &str, account_id: &str) -> String {
    format!("tokexp:{provider}:{account_id}")
}

/// Settings key for the id the *vendor* knows this account by, when its upstream
/// requires that id to travel alongside the bearer.
///
/// Codex is the case that forced it: `chatgpt.com/backend-api/codex` answers a
/// request with no `chatgpt-account-id` header with 401, which the pool reads as
/// a dead login and takes the whole account off the market. It is not a secret —
/// same class of per-account metadata as the token expiry above — so it lives in
/// `settings` rather than the secret store.
pub fn upstream_acct_key(provider: &str, account_id: &str) -> String {
    format!("upacct:{provider}:{account_id}")
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog with two vendors' models under their credential families.
    fn catalog() -> Option<SellableCatalog> {
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert("claude".to_string(), vec!["claude-opus-5".to_string()]);
        by_provider.insert("codex".to_string(), vec!["gpt-5.5".to_string(), "gpt-5.4".to_string()]);
        Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() })
    }

    #[test]
    fn a_custom_endpoint_sells_the_catalog_narrowed_to_what_it_serves() {
        // The endpoint's own list is usually far wider than the catalog — it is
        // an aggregator's whole menu — and the platform can only trade models it
        // prices. The intersection is the lane set, in both directions:
        let endpoint = ["gpt-5.5".to_string(), "claude-opus-5".to_string(), "some-model".to_string()];
        let mut sold = sellable_models(&catalog(), CUSTOM_PROVIDER, Some(&endpoint));
        sold.sort();
        assert_eq!(sold, vec!["claude-opus-5".to_string(), "gpt-5.5".to_string()]);
        // `some-model` is not traded here, and `gpt-5.4` is traded but not
        // served by this endpoint; neither may be advertised.
        assert!(!sold.contains(&"some-model".to_string()));
        assert!(!sold.contains(&"gpt-5.4".to_string()));
    }

    #[test]
    fn a_chat_only_endpoint_does_not_offer_the_responses_only_models() {
        let served = ["gpt-5.5", "gpt-5.6-luna", "claude-opus-5"].map(String::from).to_vec();
        // No `/responses` on this host: luna is listed by the aggregator and
        // still unsellable, because every tool-carrying request 400s.
        assert_eq!(
            offerable(served.clone(), true),
            vec!["gpt-5.5".to_string(), "claude-opus-5".to_string()]
        );
        // With the route it is sold like anything else, on that route.
        assert_eq!(offerable(served.clone(), false), served);
    }

    #[test]
    fn a_custom_endpoint_that_serves_nothing_advertises_nothing() {
        // Same contract as Codex's entitlement: an empty answer is a real one,
        // and a lane the upstream will refuse costs a buyer a turn and this
        // device its reputation.
        assert!(sellable_models(&catalog(), CUSTOM_PROVIDER, Some(&[])).is_empty());
        // Nothing pulled yet: a custom endpoint has no built-in fallback set,
        // because its models are its operator's, not a vendor's.
        assert!(sellable_models(&None, CUSTOM_PROVIDER, Some(&["gpt-5.5".to_string()])).is_empty());
    }

    /// Two rules meet on a Model Studio key, and both have to hold.
    ///
    /// OpenRouter prices 50 `qwen/*` ids; DashScope answers to 30 of them. The
    /// gap is not cosmetic — `qwen-2.5-72b-instruct` (Alibaba writes it without
    /// the hyphen) and the whole `qwen3-vl-*` family are third-party re-hosts
    /// Alibaba has never served — and a lane behind one of them 404s on its
    /// first request. The vendor's own `/models` is what narrows the catalog
    /// down, and it reaches this function as `entitled`.
    ///
    /// The other way round, one key there serves far more than Alibaba's own
    /// models. Filing the account under `qwen/*` — the vendor its *credential*
    /// belongs to — hid every model it resells, which is capacity the platform
    /// prices and nobody was selling.
    #[test]
    fn a_qwen_key_sells_every_vendor_alibaba_serves_and_nothing_it_does_not() {
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert(
            "qwen".to_string(),
            ["qwen3-max", "qwen3-vl-8b-instruct", "qwen3.8-max"].iter().map(|s| s.to_string()).collect(),
        );
        by_provider.insert("deepseek".to_string(), vec!["deepseek-v3.2".to_string()]);
        let catalog = Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() });
        // What `/compatible-mode/v1/models` answers, trimmed to the point: it
        // is wider than this vendor's own rows, because DashScope resells other
        // vendors' models too.
        let served = ["qwen3-max".to_string(), "qwen3.8-max".to_string(), "deepseek-v3.2".to_string()];
        let mut sold = sellable_models(&catalog, Provider::Qwen.as_str(), Some(&served));
        sold.sort();
        assert_eq!(
            sold,
            vec!["deepseek-v3.2".to_string(), "qwen3-max".to_string(), "qwen3.8-max".to_string()],
            "a model the key serves sells, whichever vendor made it"
        );
        // Priced here, never served there: advertising it would cost a buyer a
        // turn and this device its reputation.
        assert!(!sold.contains(&"qwen3-vl-8b-instruct".to_string()));
        // And an id the platform does not price at all stays out, however
        // willingly the endpoint would serve it.
        let unpriced = ["moonshot-v1-8k".to_string()];
        assert!(sellable_models(&catalog, Provider::Qwen.as_str(), Some(&unpriced)).is_empty());
    }

    /// What the platform prices, in the catalog's own spelling.
    fn tradable() -> Vec<String> {
        ["claude-haiku-4-5", "claude-opus-4-5", "gpt-5.5", "gpt-5.2", "grok-4.5"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn an_unprefixed_dotted_anthropic_id_still_finds_its_catalog_row() {
        // The b.ai shape: no vendor prefix, and Anthropic's published dotted
        // version. Guessing the vendor from the id cannot work here — there is
        // nothing to guess from — so the catalog decides, and a whole family
        // stops being invisible.
        let ids: Vec<String> =
            ["claude-opus-4.5", "gpt-5.2"].iter().map(|s| s.to_string()).collect();
        let idx = index_custom_models(&ids, &tradable());
        assert_eq!(idx.get("claude-opus-4-5").map(String::as_str), Some("claude-opus-4.5"));
        // An id the vendor already publishes dotted is left alone — OpenAI's
        // real API id *is* `gpt-5.2`.
        assert_eq!(idx.get("gpt-5.2").map(String::as_str), Some("gpt-5.2"));
    }

    /// Model Studio resells other vendors' models under the reseller's own
    /// casing. The catalog — like every vendor's own API — spells them lower,
    /// so without folding the case the newest GLM and MiniMax flagships matched
    /// nothing and were quietly unsellable on a key that serves them.
    #[test]
    fn a_resellers_capitalised_id_finds_its_lowercase_catalog_row() {
        let ids: Vec<String> = ["ZHIPU/GLM-5.3", "MiniMax/MiniMax-M3", "MiniMax-M2.5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog: Vec<String> =
            ["glm-5.3", "minimax-m3", "minimax-m2.5"].iter().map(|s| s.to_string()).collect();
        let idx = index_custom_models(&ids, &catalog);
        assert_eq!(idx.get("glm-5.3").map(String::as_str), Some("ZHIPU/GLM-5.3"));
        assert_eq!(idx.get("minimax-m3").map(String::as_str), Some("MiniMax/MiniMax-M3"));
        assert_eq!(idx.get("minimax-m2.5").map(String::as_str), Some("MiniMax-M2.5"));
        // The request still goes out under the endpoint's own spelling — only
        // the market id is folded.
        assert!(idx.values().all(|v| v.contains(char::is_uppercase)));
    }

    #[test]
    fn a_model_the_platform_does_not_price_is_left_out() {
        let ids: Vec<String> = ["meta/llama-4", "some-local-model"].iter().map(|s| s.to_string()).collect();
        assert!(index_custom_models(&ids, &tradable()).is_empty());
    }

    #[test]
    fn before_the_first_catalog_pull_the_best_guess_is_kept() {
        // Dropping everything would leave an endpoint connected a minute ago
        // advertising nothing until the next hourly refresh; `sellable_models`
        // intersects with the catalog again anyway, so a wrong guess is free.
        let ids = vec!["anthropic/claude-haiku-4.5".to_string()];
        let idx = index_custom_models(&ids, &[]);
        assert_eq!(idx.get("claude-haiku-4-5").map(String::as_str), Some("anthropic/claude-haiku-4.5"));
    }

    #[test]
    fn an_aggregators_ids_are_indexed_under_the_names_the_market_trades() {
        // The case that decides whether this feature works at all: an
        // aggregator lists vendor-prefixed, dotted ids, and the catalog trades
        // the bare hyphenated ones. Without the mapping the intersection is
        // empty and the endpoint sells nothing.
        let ids: Vec<String> = ["anthropic/claude-haiku-4.5", "openai/gpt-5.5", "x-ai/grok-4.5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog: Vec<String> =
            ["claude-haiku-4-5", "gpt-5.5", "grok-4.5"].iter().map(|s| s.to_string()).collect();
        let idx = index_custom_models(&ids, &catalog);
        // Anthropic is the one vendor whose published id is not its API id.
        assert_eq!(idx.get("claude-haiku-4-5").map(String::as_str), Some("anthropic/claude-haiku-4.5"));
        // Everyone else publishes the dotted name as the real id, so only the
        // vendor prefix comes off.
        assert_eq!(idx.get("gpt-5.5").map(String::as_str), Some("openai/gpt-5.5"));
        assert_eq!(idx.get("grok-4.5").map(String::as_str), Some("x-ai/grok-4.5"));
    }

    #[test]
    fn the_directly_hosted_id_beats_the_resellers_spelling() {
        // Model Studio lists the models it resells under both spellings, and
        // only the bare one answers without a separate subscription. Taken from
        // its live `/models`, with the catalog ids the platform trades.
        let ids: Vec<String> = ["ZHIPU/GLM-5.2", "glm-5.2", "MiniMax/MiniMax-M3", "xiaomi/mimo-v2.5-pro"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog: Vec<String> =
            ["glm-5.2", "minimax-m3", "mimo-v2.5-pro"].iter().map(|s| s.to_string()).collect();
        let idx = index_custom_models(&ids, &catalog);
        assert_eq!(idx.get("glm-5.2").map(String::as_str), Some("glm-5.2"));
        // The other two are listed only as marketplace entries, so the prefixed
        // id is the only one there is — and it still maps to its market row.
        assert_eq!(idx.get("minimax-m3").map(String::as_str), Some("MiniMax/MiniMax-M3"));
        assert_eq!(idx.get("mimo-v2.5-pro").map(String::as_str), Some("xiaomi/mimo-v2.5-pro"));
    }

    #[test]
    fn an_unprefixed_id_is_already_a_market_id() {
        // A vendor's own API — and most self-hosted gateways — list models
        // without a vendor prefix, and those ids need no translation at all.
        let idx = index_custom_models(
            &["claude-opus-5".to_string(), "gpt-5.5".to_string()],
            &["claude-opus-5".to_string(), "gpt-5.5".to_string()],
        );
        assert_eq!(idx.get("claude-opus-5").map(String::as_str), Some("claude-opus-5"));
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn a_subscription_is_unaffected_by_the_custom_arm() {
        // The catalog column still decides for every ordinary provider, and a
        // custom endpoint's presence does not widen it.
        assert_eq!(sellable_models(&catalog(), "claude", None), vec!["claude-opus-5".to_string()]);
    }

    #[test]
    fn only_the_vendors_a_subscription_can_serve_are_mapped() {
        assert_eq!(
            providers_for_vendor("anthropic"),
            &[Provider::Claude, Provider::ClaudeWork, Provider::ClaudeExtra]
        );
        assert_eq!(providers_for_vendor("openai"), &[Provider::Codex]);
        assert_eq!(providers_for_vendor("google"), &[Provider::Gemini]);
        // Both flavours of a vendor serve its catalog rows — the subscription
        // and the metered platform key differ only in which host they reach.
        assert_eq!(providers_for_vendor("moonshotai"), &[Provider::Kimi, Provider::KimiApi]);
        assert_eq!(providers_for_vendor("qwen"), &[Provider::Qwen]);
        // The catalog spells this one with a hyphen; `xai` is not the slug.
        assert_eq!(providers_for_vendor("x-ai"), &[Provider::Xai, Provider::XaiApi]);
        // DeepSeek's slug and its credential family are the same word — the
        // company ships no subscription for a metered key to be told apart from.
        assert_eq!(providers_for_vendor("deepseek"), &[Provider::Deepseek]);
        assert!(providers_for_vendor("xai").is_empty());
        // OpenRouter routing aliases are not ids any vendor API accepts.
        assert!(providers_for_vendor("~anthropic").is_empty());
        assert!(providers_for_vendor("meta-llama").is_empty());
    }

    /// Every provider the Sell page can connect must resolve to an adapter, or
    /// its accounts sit in the list doing nothing: `refresh_due_tokens` skips
    /// what it cannot build, and so does anything else keyed on the adapter.
    #[test]
    fn every_connectable_provider_has_an_adapter() {
        for p in asale_protocol::ids::subscribable_providers() {
            let a = adapter_for(p.as_str())
                .unwrap_or_else(|| panic!("`{p}` can be connected but has no adapter"));
            assert_eq!(a.provider(), p);
        }
        assert!(adapter_for("nope").is_none());
    }

    #[test]
    fn only_text_answering_models_are_sellable() {
        assert!(produces_text("text->text"));
        assert!(produces_text("text+image->text"));
        assert!(!produces_text("text->image"));
        // An unknown modality must not silently remove capacity.
        assert!(produces_text(""));
    }

    /// A cached catalog is the answer even when it lists nothing for this
    /// provider — falling back to the built-in list there would put capacity
    /// back on the market that the platform refuses to relay.
    #[test]
    fn the_built_in_list_is_only_used_before_the_first_pull() {
        let none = None;
        assert_eq!(
            sellable_models(&none, "claude", None),
            vec!["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"]
        );

        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert("claude".to_string(), vec!["claude-sonnet-5".to_string()]);
        let pulled = Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() });
        assert_eq!(sellable_models(&pulled, "claude", None), vec!["claude-sonnet-5"]);
        assert!(sellable_models(&pulled, "gemini", None).is_empty(), "the catalog's silence is an answer");
    }

    /// The ChatGPT backend that serves a Codex subscription is entitled to a
    /// slice of the platform's models and answers every other slug with
    /// `400 … "is not supported when using Codex with a ChatGPT account"` —
    /// after the request has been matched, preauthorized and routed. So the
    /// catalog is filtered by what the account itself says it may use.
    #[test]
    fn codex_only_advertises_what_the_chatgpt_account_is_entitled_to() {
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert(
            "codex".to_string(),
            vec![
                "gpt-5.1".to_string(),      // platform-only: refused upstream
                "gpt-5-codex".to_string(),  // ditto, despite the name
                "gpt-5.6-sol".to_string(),  // granted
                "gpt-5.4-mini".to_string(), // granted
            ],
        );
        let pulled = Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() });
        let granted = vec!["gpt-5.6-sol".to_string(), "gpt-5.4-mini".to_string(), "gpt-5.5".to_string()];
        assert_eq!(
            sellable_models(&pulled, "codex", Some(&granted)),
            vec!["gpt-5.6-sol", "gpt-5.4-mini"],
            "the catalog and the grant have to agree"
        );

        // Nothing granted (or nothing ever discovered) means nothing advertised:
        // a lane that can only fail is worse than no lane.
        assert!(sellable_models(&pulled, "codex", Some(&[])).is_empty());
    }

    /// OpenRouter and xAI do not spell every Grok id the same way, and an id
    /// the vendor has never heard of fails *after* the request was matched,
    /// preauthorized and routed — so the publisher wears a failure it did not
    /// cause. Those rows are dropped rather than guessed at.
    #[test]
    fn grok_ids_the_vendor_api_would_reject_are_not_advertised() {
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert(
            "xai".to_string(),
            vec![
                "grok-4.5".to_string(),
                "grok-4.3".to_string(),
                // OpenRouter's spelling; xAI serves `grok-4.20-0309-reasoning`
                // and `-non-reasoning`, and picking one would be a guess.
                "grok-4.20".to_string(),
                "grok-4.20-multi-agent".to_string(),
            ],
        );
        let pulled = Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() });
        assert_eq!(sellable_models(&pulled, "xai", None), vec!["grok-4.5", "grok-4.3"]);

        // Moonshot's ids line up with the catalog, so nothing is filtered.
        assert!(native_models("kimi").is_none());
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert("kimi".to_string(), vec!["kimi-k2.7-code".to_string()]);
        let pulled = Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() });
        assert_eq!(sellable_models(&pulled, "kimi", None), vec!["kimi-k2.7-code"]);
    }

    /// DeepSeek answers to two model strings and the catalog lists ten under
    /// its slug: the dated re-posts its own pointers resolve to, and the V3/R1
    /// back catalogue an aggregator can still route. A key that advertised
    /// those would be matched and then refused by its own vendor.
    #[test]
    fn deepseek_advertises_only_the_two_ids_its_api_accepts() {
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert(
            "deepseek".to_string(),
            vec![
                "deepseek-v4-flash".to_string(),
                // What `deepseek-v4-flash` currently resolves to — a real
                // catalog row, and not a string the vendor takes.
                "deepseek-v4-flash-0731".to_string(),
                "deepseek-v4-pro".to_string(),
                "deepseek-v3.2".to_string(),
            ],
        );
        let pulled = Some(SellableCatalog { fetched_at: 1, by_provider, ..Default::default() });
        assert_eq!(
            sellable_models(&pulled, "deepseek", None),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );
    }

    /// The offline fallback has to satisfy the same rule as the catalog: it is
    /// advertised before anything has been pulled, so a stale id there puts
    /// unusable capacity on the market for as long as the device is offline.
    #[test]
    fn the_built_in_lists_only_name_ids_the_vendor_serves() {
        for provider in ["xai", "xai_api", "deepseek"] {
            let native = native_models(provider).unwrap();
            for m in fallback_models(provider) {
                assert!(native.contains(m), "`{m}` is not an id the {provider} API serves");
            }
        }
        assert!(!fallback_models("kimi").is_empty(), "a fresh Kimi install must have something to sell");
    }

    // ── The sell gate ───────────────────────────────────────────────────────

    use asale_client_core::store::LocalStore;

    /// A store holding one sell-enabled Claude account that has served
    /// `sold` tokens, with `claude-opus-5` and `claude-sonnet-5` tradable.
    async fn gated_store(sold: i64) -> LocalStore {
        let s = LocalStore::open_memory().await.unwrap();
        s.upsert_tool("claude", "a@b.io", "asale:claude:a@b.io", &["oauth"], "oauth").await.unwrap();
        s.set_tool_sell("claude", "a@b.io", true, 0).await.unwrap();
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert(
            "claude".to_string(),
            vec!["claude-opus-5".to_string(), "claude-sonnet-5".to_string()],
        );
        let cat = SellableCatalog { fetched_at: now_secs(), by_provider, ..Default::default() };
        s.set_setting(CATALOG_KEY, &serde_json::to_string(&cat).unwrap()).await.unwrap();
        s.insert_provider_record("t1", "claude", "a@b.io", "claude-opus-5", sold, 0, 0, 0, "ok")
            .await
            .unwrap();
        s
    }

    /// Bank a reading the way `commands::usage` does.
    async fn bank(s: &LocalStore, at: i64, windows: serde_json::Value) {
        let snap = serde_json::json!({ "at": at, "windows": windows });
        s.set_setting("quota_snapshot:claude:a@b.io", &snap.to_string()).await.unwrap();
    }

    fn quota_of(pool: &StdMutex<AccountPool>) -> u64 {
        pool.lock().unwrap().lane_views(now_secs())[0].quota_remaining
    }

    fn status_of(pool: &StdMutex<AccountPool>) -> String {
        pool.lock().unwrap().statuses(now_secs())[0].status.clone()
    }

    /// The production failure this whole path was rebuilt for: a device that has
    /// sold far past every guessed plan cap — with no reading banked at all —
    /// keeps selling, because nothing local is entitled to that verdict.
    ///
    /// Before, `plan_window_cap` (220k for a Claude login, which never carries a
    /// plan) minus local sales declared the subscription spent. In production
    /// that pulled all six Claude lanes of the only seller of `claude-opus-5`
    /// off the market for a minute and a half at a time, several times a day,
    /// while Anthropic's own utilisation reading did not move at all.
    #[tokio::test]
    async fn selling_past_every_guessed_cap_no_longer_stops_the_account() {
        let store = gated_store(50_000_000).await;
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "available", "50M sold against a 220k guess, still selling");
        assert_eq!(quota_of(&pool), asale_client_core::pool::UNCAPPED);
    }

    /// A reading that says the account has room changes nothing either — it is
    /// not converted into a token headroom that local sales can eat through.
    #[tokio::test]
    async fn a_reading_with_headroom_is_never_turned_back_into_a_token_budget() {
        let store = gated_store(50_000_000).await;
        let now = now_secs();
        bank(
            &store,
            now,
            serde_json::json!([
                {"key":"5h","used_percent":3.0,"reset_at":now + 9_000,"window_seconds":18_000},
                {"key":"7d","used_percent":77.0,"reset_at":now + 200_000,"window_seconds":604_800},
            ]),
        )
        .await;
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "available");
        // 23% of a guessed 4.4M would have been ~1M tokens, and this device has
        // sold fifty times that since the reading. That subtraction is what used
        // to empty the market; it no longer exists.
        assert_eq!(quota_of(&pool), asale_client_core::pool::UNCAPPED);
    }

    /// The operator's own daily cap is the one local rule left, and it still
    /// stops the whole account — with the rollover as its clock.
    #[tokio::test]
    async fn the_operators_own_daily_cap_still_stops_the_account() {
        let store = gated_store(500_000).await;
        store.set_tool_sell("claude", "a@b.io", true, 400_000).await.unwrap();
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "exhausted");
        assert_eq!(quota_of(&pool), 0);
        let back = pool.lock().unwrap().next_auto_resume(now_secs());
        assert_eq!(back, Some(crate::commands::day_start_ts() + 86_400));
    }

    /// The vendor's *own* verdict still stops the sale: a window it reports as
    /// fully spent is not an estimate, and selling into it only burns buyer
    /// requests on 429s we were told about in advance. It carries the instant it
    /// comes back, too.
    #[tokio::test]
    async fn a_window_the_vendor_reports_as_spent_stops_selling_and_says_when_it_returns() {
        let store = gated_store(1_000).await;
        let now = now_secs();
        bank(
            &store,
            now,
            serde_json::json!([{"key":"5h","used_percent":100.0,"reset_at":now + 3_600,"window_seconds":18_000}]),
        )
        .await;
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "exhausted");
        // And the daemon wakes for it rather than waiting out the next rebuild.
        assert_eq!(pool.lock().unwrap().next_auto_resume(now), Some(now + 3_600));
    }

    /// Opus's own weekly window is spent; Sonnet's is not. One lane leaves the
    /// market, the other keeps selling — which is the whole reason scoped
    /// windows are kept apart from the account-wide ones.
    #[tokio::test]
    async fn a_spent_opus_window_leaves_the_rest_of_the_subscription_selling() {
        let store = gated_store(1_000).await;
        let now = now_secs();
        bank(
            &store,
            now,
            serde_json::json!([
                {"key":"5h","used_percent":10.0,"reset_at":now + 9_000,"window_seconds":18_000},
                {"key":"7d_opus","used_percent":100.0,"reset_at":now + 50_000,"window_seconds":604_800},
            ]),
        )
        .await;
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "available", "the account itself is fine");
        let views = pool.lock().unwrap().lane_views(now);
        let lane = |m: &str| views.iter().find(|v| v.model == m).unwrap().clone();
        assert_eq!(lane("claude-opus-5").status, "paused");
        assert_eq!(lane("claude-opus-5").paused_reason.as_deref(), Some("quota"));
        assert_eq!(lane("claude-opus-5").resume_at, now + 50_000);
        assert_eq!(lane("claude-sonnet-5").status, "selling");
    }

    /// The account whose lanes this whole rewrite is about: a Max 20×
    /// subscription that Anthropic reports as 7% / 54% spent. It sells, and it
    /// declares a flat window rather than a number derived from a guessed cap.
    #[tokio::test]
    async fn the_production_account_that_was_selling_nothing_stays_on_the_market() {
        let store = gated_store(238_734).await;
        let now = now_secs();
        // What `quota_poll::refresh_plan` writes after reading the profile.
        store.set_setting("plan:claude:a@b.io", "max20").await.unwrap();
        bank(
            &store,
            now,
            serde_json::json!([
                {"key":"5h","used_percent":7.0,"reset_at":now + 17_000,"window_seconds":18_000},
                {"key":"7d","used_percent":54.0,"reset_at":now + 240_000,"window_seconds":604_800},
            ]),
        )
        .await;
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "available");
        assert_eq!(quota_of(&pool), asale_client_core::pool::UNCAPPED);
        // And every lane of it is on the market, Opus included — nothing in
        // this reading is scoped to a model.
        let views = pool.lock().unwrap().lane_views(now);
        assert!(views.iter().all(|v| v.status == "selling"), "{views:?}");
        // What the market is told: a flat, finite window, the same one a metered
        // key declares. Not `guessed cap × headroom`, which is the number that
        // used to run out mid-afternoon.
        let items = build_supply_items(&store, &pool).await;
        let first = items.as_array().unwrap().first().unwrap();
        assert_eq!(first["window_remaining"].as_i64(), Some(CUSTOM_WINDOW_TOKENS as i64));
        assert_eq!(first["available"].as_bool(), Some(true));
    }

    /// A reserve above the market is an *offer*, and the market only finds it
    /// if the lane is declared. Hiding it client-side is what answered a buyer
    /// "nobody is selling this" on 2026-08-26 while endpoints stood ready at
    /// 60–90% against a 34% market: the gateway logged `withheld_ask_pct: 0`
    /// because a lane it was never told about has no ask to escalate to.
    #[tokio::test]
    async fn a_reserve_above_the_market_is_declared_as_an_ask() {
        let store = gated_store(0).await;
        store.set_tool_ratio_band("claude", "a@b.io", 90, 100).await.unwrap();
        // What the market pays: far below this seller's floor.
        let mut by_provider = std::collections::BTreeMap::new();
        by_provider.insert("claude".to_string(), vec!["claude-opus-5".to_string()]);
        let cat = SellableCatalog {
            fetched_at: now_secs(),
            by_provider,
            ratios: [("claude-opus-5".to_string(), 34)].into_iter().collect(),
            priced_at: now_secs(),
        };
        store.set_setting(CATALOG_KEY, &serde_json::to_string(&cat).unwrap()).await.unwrap();

        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        let items = build_supply_items(&store, &pool).await;
        let opus = items
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["model"] == "claude-opus-5")
            .expect("the lane is declared, not withheld");
        assert_eq!(opus["available"].as_bool(), Some(true));
        assert_eq!(opus["ask_ratio"].as_i64(), Some(90), "the reserve is the offer");
        assert!(opus["window_remaining"].as_i64().unwrap_or(0) > 0, "with capacity behind it");
    }

    /// A stale reading is simply no reading: it stops naming scope blocks and
    /// stops speaking for the account. What it must *not* do is fall back to a
    /// local estimate — there is none to fall back to any more.
    #[tokio::test]
    async fn a_stale_reading_leaves_the_account_selling_rather_than_guessing() {
        let store = gated_store(239_000).await;
        let now = now_secs();
        bank(
            &store,
            now - crate::commands::usage::GATE_SNAPSHOT_MAX_AGE - 60,
            serde_json::json!([{"key":"5h","used_percent":100.0,"reset_at":now + 9_000,"window_seconds":18_000}]),
        )
        .await;
        let pool = StdMutex::new(AccountPool::new(asale_client_core::Strategy::FillFirst));
        rebuild_pool(&store, &pool).await;
        assert_eq!(status_of(&pool), "available");
    }
}
