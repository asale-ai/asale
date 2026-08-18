//! asale wsrelay wire contract.
//!
//! This crate exists because the protocol used to be written down twice — once
//! in `asale-server/src/wsrelay/protocol.rs` and once in
//! `asale-client/core/src/protocol.rs` — with a third partial copy of the
//! payload types in the server's `model.rs`, and a fourth in the client's
//! publisher, which hand-built the supply declaration with `json!` and had to
//! spell every field name correctly by hand.
//!
//! Nothing in that arrangement fails to compile when the two sides drift: they
//! are separate crates, so a renamed field or a changed constant just produces
//! a gateway and a publisher that no longer understand each other, at runtime,
//! in production. One definition, depended on by both, is the only thing that
//! makes that class of bug impossible.
//!
//! Deliberately dependency-light (serde + serde_json + uuid): both a Postgres
//! server and a Tauri desktop client link it, so it must not drag a runtime,
//! an HTTP stack or a database driver behind it.

pub mod frame;
pub mod ids;
pub mod payload;
pub mod providers;

pub use frame::{codes, is_retriable, Envelope, MAX_FRAME_BYTES};
pub use ids::{Provider, TokenType, Vendor};
pub use providers::{spec, spec_of, Credential, ProviderSpec, QuotaSource, WindowCap, PROVIDERS};
pub use payload::{ControlPayload, HttpRequestPayload, SupplyItem, UpstreamPayload, Usage};
