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
    )
}
