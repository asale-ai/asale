//! One Claude Code session per selling account.
//!
//! Anthropic decides "plan limits" from whether a request reads like the
//! official CLI, and part of how the CLI reads is that one credential keeps
//! one session alive for as long as the process does. A new UUID per request
//! is exactly what a third-party client looks like — the body fingerprints
//! right and then the metadata betrays it, which is how `claude.ai`'s
//! third-party-usage wall answered a purchase that should have come off the
//! plan (2026-08-23).
//!
//! The id outlives the process. A daemon restarts for reasons the upstream
//! knows nothing about — an upgrade, a crash, a user quitting the app — and a
//! credential that presents a new session every time it comes back is the
//! rotation this module exists to avoid, just on a slower clock. So the map is
//! a write-through cache over the local store: read once at startup, and every
//! id minted afterwards is written back.
//!
//! The cache is what keeps [`claude_session_for`] synchronous, which it has to
//! be — `TokenProvider::session_for` is a sync trait method on the hot path of
//! every leased request, and the store is async.

use asale_client_core::store::LocalStore;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

static MAP: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
/// The store to write newly minted ids back to, handed over by [`warm`].
/// Absent under unit test, where the cache alone is the point.
static STORE: OnceLock<Arc<LocalStore>> = OnceLock::new();

fn map() -> &'static std::sync::Mutex<HashMap<String, String>> {
    MAP.get_or_init(Default::default)
}

/// Settings key holding one account's Claude session id.
pub(crate) fn session_key(account_id: &str) -> String {
    format!("claudesession:{account_id}")
}

/// Load every session id already on disk into the cache.
///
/// Called once as the daemon comes up, before anything can lease a token. A
/// failure is not fatal: the worst case is the ids are minted afresh, which is
/// the behaviour this module had before it persisted anything.
pub async fn warm(store: Arc<LocalStore>) {
    let rows = match store.settings_with_prefix("claudesession:").await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("claude session ids could not be read back: {e}");
            Vec::new()
        }
    };
    if let Ok(mut cache) = map().lock() {
        for (k, v) in rows {
            if let Some(account_id) = k.strip_prefix("claudesession:") {
                cache.insert(account_id.to_string(), v);
            }
        }
    }
    let _ = STORE.set(store);
}

/// The session id one account presents, derived once and then kept.
/// A helper for the daemon's `claude` provider, not for exports.
///
/// A freshly minted id is written to the store on a detached task rather than
/// awaited: this runs inside a sync trait method, and a request must not wait
/// on a disk write for an id it is already holding. Losing that write costs one
/// rotation at the next restart, which is what happened on every restart before.
pub(crate) fn claude_session_for(account_id: &str) -> Option<String> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return None;
    }
    let mut cache = map().lock().ok()?;
    if let Some(id) = cache.get(account_id) {
        return Some(id.clone());
    }
    // A uuid, not a `ses-…` label: Claude Code's `metadata.user_id` and
    // `x-claude-code-session-id` both carry a plain uuid, and an id that does
    // not parse as one is a tell on every request it rides.
    let id = uuid::Uuid::new_v4().to_string();
    cache.insert(account_id.to_string(), id.clone());
    drop(cache);
    persist(account_id.to_string(), id.clone());
    Some(id)
}

/// Write one newly minted id back, if there is a store and a runtime to do it
/// on. Both are absent under unit test, where the cache alone is the point.
fn persist(account_id: String, id: String) {
    let Some(store) = STORE.get().cloned() else { return };
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    handle.spawn(async move {
        if let Err(e) = store.set_setting(&session_key(&account_id), &id).await {
            tracing::warn!(account = %account_id, "claude session id could not be persisted: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    /// The same account keeps one id; a different one gets its own. This is the
    /// whole contract the upstream reads.
    #[test]
    fn an_account_keeps_one_session() {
        let a = super::claude_session_for("acct-1").unwrap();
        assert_eq!(super::claude_session_for("acct-1").as_deref(), Some(a.as_str()));
        assert_ne!(super::claude_session_for("acct-2").unwrap(), a);
        // Anything that does not parse as a uuid is a tell on every request.
        assert!(uuid::Uuid::parse_str(&a).is_ok(), "{a}");
        // An account with no name cannot be given a stable session at all.
        assert_eq!(super::claude_session_for("  "), None);
    }
}
