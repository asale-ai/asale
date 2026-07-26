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
    )
}
