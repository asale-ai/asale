//! asale-client-core — the non-GUI core of the asale desktop client. The Tauri
//! layer (src-tauri) wraps this with commands/events; this crate holds the
//! wsrelay client, provider executor, device identity, discovery adapters, and
//! local store. Everything here is unit-testable without a GUI.
//!
//! The wsrelay protocol here mirrors the server's; the server's e2e test
//! already exercises a publisher speaking these exact frames.

pub mod cli_import;
pub mod config;
pub mod device_flow;
pub mod discovery;
pub mod executor;
pub mod http;
pub mod pool;
pub mod protocol;
pub mod reconcile;
pub mod security;
pub mod store;
pub mod ws;

pub use discovery::{
    estimate_quota_window, needs_refresh, plan_window_cap, ApiKeyAdapter, ClaudeAdapter, CodexAdapter,
    DeviceFlowAdapter, GeminiAdapter, Provider, QuotaWindow, RefreshedToken, ToolAdapter, Wire,
};
pub use executor::{LeasedToken, RecordSink, TaskOutcome, TokenProvider};
pub use pool::{
    AccountPool, AccountRuntime, LaneState, LaneStatusView, PauseReason, PickedAccount, Strategy,
    UpstreamErrorKind,
};
pub use security::DeviceIdentity;
pub use ws::{
    next_backoff, spawn_publisher, ConfigSource, ConnState, LaneControl, PublisherDeps, PublisherHandle,
    SupplySource, WsConfig,
};
