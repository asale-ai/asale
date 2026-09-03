//! Message envelope, frame type tags, size limit and error codes (spec §2.2/§2.3/§2.6).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Uniform frame envelope for every message in both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nonce: String,
    #[serde(default)]
    pub payload: Value,
}

impl Envelope {
    pub fn new(msg_type: &str, payload: Value) -> Envelope {
        Envelope {
            id: uuid::Uuid::new_v4().to_string(),
            msg_type: msg_type.to_string(),
            ts: 0,
            nonce: String::new(),
            payload,
        }
    }

    pub fn with_id(id: &str, msg_type: &str, payload: Value) -> Envelope {
        Envelope {
            id: id.to_string(),
            msg_type: msg_type.to_string(),
            ts: 0,
            nonce: String::new(),
            payload,
        }
    }
}

/// Maximum WS message **and** frame size, in bytes, for both directions.
///
/// Advertised to publishers as `max_frame` in `hello.ack` and enforced on both
/// ends — the server sets it on the upgrade, the client's `ws.rs` mirrors it on
/// connect. Both sides used to fall back to the tungstenite default of 16 MiB
/// per frame while `hello.ack` advertised 64 MB, so a large enough
/// `http_request` killed the whole publisher session (a frame-size violation is
/// a protocol error, not a per-task failure) rather than failing one request.
///
/// A request travels as ONE message: its body is base64'd (≈4/3 expansion) into
/// a single `http_request`, so this is the ceiling the gateway's request body
/// limit (`GatewayConfig::max_request_bytes`) has to stay under. At that
/// setting's 100 MiB default the frame lands near 134 MiB, which is what sizes
/// this constant — raise `ASALE_MAX_REQUEST_BYTES` past ~140 MiB and this has
/// to move with it.
pub const MAX_FRAME_BYTES: usize = 192 * 1024 * 1024;

// ── Frame type tags ─────────────────────────────────────────────────

pub const T_HELLO: &str = "hello";
pub const T_HELLO_ACK: &str = "hello.ack";
pub const T_HEARTBEAT: &str = "heartbeat";
pub const T_HEARTBEAT_ACK: &str = "heartbeat.ack";
pub const T_SUPPLY_DECLARE: &str = "supply.declare";
pub const T_SUPPLY_UPDATE: &str = "supply.update";
/// Publisher → gateway: "my operator fixed this lane, let it back in".
/// Payload `{"model": "..."}`; an empty/absent model resumes every lane of the
/// device. The only thing that clears a quarantine (§4.5).
pub const T_SUPPLY_RESUME: &str = "supply.resume";
pub const T_HTTP_REQUEST: &str = "http_request";
pub const T_STREAM_START: &str = "stream_start";
pub const T_STREAM_CHUNK: &str = "stream_chunk";
pub const T_STREAM_END: &str = "stream_end";
pub const T_HTTP_RESPONSE: &str = "http_response";
pub const T_ERROR: &str = "error";
pub const T_PRICE_UPDATE: &str = "price.update";
pub const T_SETTLE_NOTIFY: &str = "settle.notify";
pub const T_CONTROL: &str = "control";
pub const T_PING: &str = "ping";
pub const T_PONG: &str = "pong";

// ── Handshake (WS upgrade request) ──────────────────────────────────
//
// The publisher authenticates in the upgrade headers, before the socket
// exists. The signed body is
//
//     device_id | ts_ms | nonce | audience | challenge
//
// `audience` and `challenge` are what stop a captured handshake from being
// useful anywhere else:
//
//   * `audience` names the gateway the publisher *meant* to reach, and the
//     gateway checks it against its own identity — so a signature collected by
//     a peer impersonating one gateway cannot be forwarded to another.
//   * `challenge` is issued by that gateway ([`WS_CHALLENGE_PATH`]), valid
//     once, and burned on use — so a captured handshake cannot be replayed at
//     all, rather than merely being hard to replay inside a clock-skew window.

/// `GET` here for a one-shot challenge before opening the socket.
/// Responds `{"challenge": "<opaque>", "expires_in": <secs>}`.
pub const WS_CHALLENGE_PATH: &str = "/v1/ws/challenge";

pub const H_DEVICE: &str = "x-asale-device";
pub const H_TS: &str = "x-asale-ts";
pub const H_NONCE: &str = "x-asale-nonce";
pub const H_SIG: &str = "x-asale-sig";
pub const H_AUDIENCE: &str = "x-asale-audience";
pub const H_CHALLENGE: &str = "x-asale-challenge";

/// The client's own build version, so the platform can refuse to trade with a
/// release too old to behave. Sent on the WS upgrade and on every HTTP call to
/// asale's own hosts — not to providers, who have no business knowing.
///
/// Carried in a header of its own rather than left to `User-Agent` because the
/// upgrade request has no `User-Agent` to piggyback on, and because a header
/// that means exactly one thing survives a UA-string reformat.
pub const H_CLIENT_VERSION: &str = "x-asale-client-version";

/// Buy-side request header naming the one device that may serve it.
///
/// Matching normally chooses, and that is the whole point of a market — so this
/// exists for the single question a market cannot answer: *is my own selling
/// working?* A seller watching a lane sit at "online, earning nothing" has no
/// way to tell an upstream that has stopped answering apart from a market that
/// simply has not sent them anything, and every indirect test (probing the
/// vendor by hand, waiting for a buyer) checks something other than the path a
/// real request takes.
///
/// So the test is a real buy that happens to be pinned: same key, same gateway,
/// same catalog and price checks, same relay frame, same executor on the seller
/// side, same metering afterwards. Only the choice of lane is taken away from
/// the matcher.
///
/// **The gateway must verify the named device belongs to the caller** (see
/// `relay::handle_inner`). Unverified, this header would be a way to aim
/// requests at a stranger's subscription — picking out one seller to drain, or
/// to fail repeatedly until their reputation collapses. Owning both sides makes
/// it a self-test and nothing else: the tokens come off the caller's own
/// subscription window, which is exactly what they are trying to measure.
pub const H_TARGET_DEVICE: &str = "x-asale-target-device";

/// Buy-side request header naming the tool the request came from.
///
/// Every buy used to be the same request whoever sent it, and one thing broke
/// that: a vendor may refuse traffic from a client that is not its own. A Claude
/// subscription bearer is confined to Anthropic's own products, so serving an
/// opencode buyer from a subscription lane buys a refusal and risks the seller's
/// account — see `providers::denied_providers`, which is the whole rule and the
/// only thing this header feeds.
///
/// Set by the buy proxy from its own `/{tool}` addressing, which is how it
/// already knows whose switch and model list to enforce. It is a routing hint
/// and never an identity claim: the request is authorized by the api key, an
/// unrecognised value means the same as no value, and the worst a caller can do
/// by lying is narrow the supply *they* are matched against.
pub const H_TOOL: &str = "x-asale-tool";

/// Buy-side request header carrying the caller's own price ceiling, in whole
/// percent of the vendor's list price.
///
/// The market ratio moves without asking anyone: a request that finds no supply
/// at the published price raises it to wherever the cheapest reserve is, and the
/// buyer pays that. This is the number above which the buyer would rather not
/// trade at all — the gateway refuses instead of matching.
///
/// Sent by the local buy proxy, which is where a desktop user's setting lives.
/// A caller reaching the gateway with a bare api key has no proxy to set it, so
/// the ceiling written on the key itself applies instead (`api_keys.max_ratio_pct`).
/// Never widening: a value above the key's own ceiling is ignored, so the header
/// can only ever make a request pickier than the key already is.
pub const H_MAX_RATIO: &str = "x-asale-max-ratio";

/// The exact bytes both sides sign and verify. Defined once so the two can
/// never disagree about field order or separator.
pub fn handshake_signing_body(
    device_id: &str,
    ts_ms: i64,
    nonce: &str,
    audience: &str,
    challenge: &str,
) -> String {
    format!("{device_id}|{ts_ms}|{nonce}|{audience}|{challenge}")
}

/// Error codes (spec §2.6).
pub mod codes {
    pub const AUTH_FAILED: &str = "AUTH_FAILED";
    pub const QUOTA_SIG_INVALID: &str = "QUOTA_SIG_INVALID";
    pub const UPSTREAM_4XX: &str = "UPSTREAM_4XX";
    pub const UPSTREAM_5XX: &str = "UPSTREAM_5XX";
    pub const UPSTREAM_RATE_LIMIT: &str = "UPSTREAM_RATE_LIMIT";
    pub const TOKEN_EXPIRED: &str = "TOKEN_EXPIRED";
    pub const BUDGET_EXCEEDED: &str = "BUDGET_EXCEEDED";
    pub const CHUNK_GAP: &str = "CHUNK_GAP";
    pub const INTERNAL: &str = "INTERNAL";
    /// The lane cannot serve this request, but another one can: the upstream
    /// does not publish this model to that account, or it refused the machine
    /// the account is running on (a region block, a middlebox).
    ///
    /// Its own code because the alternatives both lie. `UPSTREAM_4XX` says the
    /// buyer's request was bad — which strands them on a seller whose problem
    /// they cannot fix — and `TOKEN_EXPIRED` sends the seller to re-authenticate
    /// a credential that is working. Retriable, and a fault of the lane: the
    /// gateway transfers the request and penalises the lane it left.
    pub const LANE_UNUSABLE: &str = "LANE_UNUSABLE";
    /// The publisher's WS session went away with this task still in flight.
    ///
    /// Only ever produced by the gateway, never sent by a publisher — a client that
    /// could report this would by definition still be connected. It exists so the
    /// case has a name in the failover path and in the client console, instead of
    /// the request hanging until something else times out.
    pub const PUBLISHER_GONE: &str = "PUBLISHER_GONE";
    /// The publisher's session is alive but it has produced no frame for this task
    /// for longer than the gateway is willing to wait. Also gateway-only.
    pub const PUBLISHER_STALLED: &str = "PUBLISHER_STALLED";
    /// The publisher ended the turn cleanly having produced nothing billable.
    ///
    /// Gateway-only, and it exists because this was the one way a task could
    /// reach `status=3` carrying no code and no message at all: `finalize` files
    /// an ending of `Complete` with zero usage as a failure, and until now wrote
    /// nothing beside it. On the operator's transaction page that is a row
    /// saying "failed" and refusing to say why — two of them on 2026-08-31, both
    /// unanswerable after the fact.
    ///
    /// Deliberately absent from [`is_retriable`]: by the time it is known the
    /// turn ran to its own end, so there is nothing to hand on. It is a label
    /// for the record, not a routing decision.
    pub const EMPTY_COMPLETION: &str = "EMPTY_COMPLETION";
}

/// Whether a 4xx from a provider means "this account cannot pay for the call"
/// rather than "this request was bad".
///
/// Every vendor spells it in a different code, and mostly not the one HTTP
/// reserved for it: Anthropic hides a subscription wall behind a `400` whose
/// body mentions extra usage, OpenAI-compatible aggregators answer `400`
/// `insufficient_user_quota`, OpenRouter answers `402` with a link to its
/// top-up page. Read as an ordinary bad request they all point at the buyer —
/// the lane keeps its supply entry, wins the next match and fails it again.
/// On 2026-08-25, 179 of one device's 220 failures over two days were a single
/// aggregator key with an empty balance, each one a buyer's request lost.
///
/// Lives here because both ends have to agree: the publisher cools the account
/// on it, and the gateway re-reads the frame so sellers still on an older
/// client are covered too.
pub fn is_out_of_credit(status: u16, body: &str) -> bool {
    // The one status that means exactly this and nothing else.
    if status == 402 {
        return true;
    }
    if !(400..500).contains(&status) {
        return false;
    }
    let body = body.to_ascii_lowercase();
    [
        // Anthropic, on a subscription whose plan allowance is spent.
        "draw from your extra usage",
        // OpenAI-compatible aggregators (Model Studio, 302, and friends).
        "insufficient_user_quota",
        "insufficient balance",
        // OpenAI's own name for it.
        "insufficient_quota",
        "exceeded your current quota",
        // OpenRouter.
        "add more credits",
        "add credits",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

/// The words a vendor uses when it is talking about the *credential* rather
/// than about the request. Deliberately narrow: a geo refusal or a malformed
/// body never contains them.
const CREDENTIAL_MARKERS: [&str; 7] = [
    "authentication_error",
    "permission_error",
    "invalid_api_key",
    "invalid_token",
    "expired",
    "api key",
    "oauth",
];

/// Whether a 4xx means "this account's credential is wrong", whatever status
/// the vendor chose to say it in.
///
/// `401` is the status that means exactly this; the rest have to be read. xAI
/// answers an invalid key with a `400 invalid-argument` — which the catch-all
/// reads as "the buyer's request was bad", so the lane keeps its supply entry,
/// wins the next match and fails it again, and the buyer is never handed to a
/// seller whose key works. On 2026-09-02/03 one xAI key did that 33 times
/// across `grok-4.5` and `grok-build-0.1`, every one a lost request.
///
/// Lives here for the same reason [`is_out_of_credit`] does: the publisher
/// flags the account on it, and the gateway re-reads the frame so sellers on an
/// older client are covered without waiting for uptake.
pub fn is_bad_credential(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if !(400..500).contains(&status) {
        return false;
    }
    let b = body.to_ascii_lowercase();
    CREDENTIAL_MARKERS.iter().any(|m| b.contains(m))
}

/// Whether an error code is retriable (drives failure transfer).
pub fn is_retriable(code: &str) -> bool {
    matches!(
        code,
        codes::UPSTREAM_5XX
            | codes::UPSTREAM_RATE_LIMIT
            | codes::TOKEN_EXPIRED
            | codes::CHUNK_GAP
            | codes::INTERNAL
            // A publisher that vanished or went silent is the clearest case there
            // is for handing the request to somebody else: nothing about it says
            // the request itself is bad.
            | codes::PUBLISHER_GONE
            | codes::PUBLISHER_STALLED
            // Nothing about it says the request is bad — only that this lane
            // is the wrong one to ask.
            | codes::LANE_UNUSABLE
            // A publisher that cannot verify the grant it was handed is one
            // whose own machine is misconfigured — no quota public key injected,
            // or a clock far enough off that a valid grant reads as expired. It
            // refuses *every* task dispatched to it, so leaving the buyer on it
            // fails a request the seller next door would have served. If the
            // fault is really ours (a broken signing seed), the next seller
            // refuses it the same way and the attempt budget ends the search.
            | codes::QUOTA_SIG_INVALID
    )
}
