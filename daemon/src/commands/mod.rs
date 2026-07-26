//! Command implementations (spec §2.1) — the real native + server logic, with
//! no GUI dependency. The HTTP layer (`rpc.rs`) maps `POST /rpc/{cmd}` onto
//! these functions; the Tauri shell and any browser call the same API.
//!
//! Browser OAuth flows are two-step here (unlike the old in-process Tauri
//! commands): `oauth_login` / `platform_oauth_login` return `{flow_id,
//! auth_url}` immediately, the frontend opens the URL (or the daemon opens the
//! local browser when asked), and the frontend polls `oauth_result` until the
//! loopback callback + token exchange complete.
//!
//! One module per area of the app. They were a single 1900-line file, which
//! made "where does the buy switch live" a search rather than a lookup, and put
//! the OAuth flows, the ledger, the CLI scanner and the usage rollups all in
//! one another's blast radius.
//!
//!   [`server_client`] — REST calls to the asale server; everything else goes
//!                       through its `authed` helper
//!   [`auth`]          — register / sign in / sign out / profile
//!   [`oauth_flow`]    — the two browser OAuth flows
//!   [`wallet`]        — balance, deposits, withdrawals, consumer API keys
//!   [`settings`]      — daemon identity, upstream proxy, consume mode
//!   [`sell`]          — publishing switch, devices, publisher policy
//!   [`accounts`]      — subscription discovery/import + per-account sell rules
//!   [`buy`]           — pointing a local AI CLI at the proxy, and back
//!   [`records`]       — task records and reconciliation
//!   [`usage`]         — sold/bought/used rollups and rate-limit windows
//!
//! The re-exports below keep the flat `commands::login` paths `rpc.rs` uses, so
//! the split is invisible to callers.

pub mod accounts;
pub mod auth;
pub mod buy;
pub mod oauth_flow;
pub mod records;
pub mod sell;
pub mod server_client;
pub mod settings;
pub mod usage;
pub mod wallet;

pub use accounts::*;
pub use auth::*;
pub use buy::*;
pub use oauth_flow::*;
pub use records::*;
pub use sell::*;
pub use settings::*;
pub use usage::*;
pub use wallet::*;

/// Every command returns either a JSON value or a message the frontend shows
/// verbatim, so the error type is the message itself.
pub type R<T> = Result<T, String>;

pub(crate) fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub(crate) fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Unix-seconds timestamp for the start of the current UTC day.
pub(crate) fn day_start_ts() -> i64 {
    let n = now_secs();
    n - n.rem_euclid(86400)
}

/// Format a unix-seconds timestamp as a UTC `YYYY-MM-DD` string (civil date,
/// matching SQLite's `strftime('%Y-%m-%d', ts, 'unixepoch')` — no chrono dep).
pub(crate) fn day_str(ts: i64) -> String {
    let z = ts.div_euclid(86400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
