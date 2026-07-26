//! wsrelay protocol — the client's name for the shared `asale-protocol` crate.
//!
//! These types used to be a hand-kept mirror of the server's
//! `src/wsrelay/protocol.rs`. Both sides now compile the same definitions, so a
//! field the server renames stops the client from building instead of quietly
//! producing a publisher the gateway no longer understands.

pub use asale_protocol::frame::{
    codes, handshake_signing_body, is_retriable, Envelope, H_AUDIENCE, H_CHALLENGE, H_DEVICE,
    H_NONCE, H_SIG, H_TS, MAX_FRAME_BYTES, T_CONTROL, T_ERROR, T_HEARTBEAT, T_HEARTBEAT_ACK,
    T_HELLO, T_HELLO_ACK, T_HTTP_REQUEST, T_HTTP_RESPONSE, T_PING, T_PONG, T_PRICE_UPDATE,
    T_SETTLE_NOTIFY, T_STREAM_CHUNK, T_STREAM_END, T_STREAM_START, T_SUPPLY_DECLARE,
    T_SUPPLY_RESUME, T_SUPPLY_UPDATE, WS_CHALLENGE_PATH,
};
pub use asale_protocol::payload::{
    ControlPayload, HttpRequestPayload, SupplyItem, UpstreamPayload, Usage,
};
