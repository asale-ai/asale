//! Per-account credential isolation for the sell side, modeled on
//! CLIProxyAPI's `auth-dir` (`internal/auth/*/filename.go`).
//!
//! CLIProxyAPI never shares state with the vendor CLI it borrowed a login from:
//! every account gets its own file under the proxy's *own* directory, named
//! `<provider>-<hash>-<email>[-<plan>].json`, and the proxy refreshes only that
//! copy. Two accounts of the same provider therefore never collide, and the
//! locally installed CLI's `~/.claude/.credentials.json` is left alone.
//!
//! asale follows the same shape:
//!
//!   `~/.asale/auths/<provider>-<hash8>-<account>[-<plan>].json`
//!
//! with one difference — the bearer and refresh tokens are **not** written here.
//! They stay in the encrypted secret store (`keychain.rs`), and this file holds
//! only the non-secret manifest plus the two secret-store keys that name them.
//! So the directory is safe to inspect, `chmod 0600` regardless, and there is
//! exactly one manifest per (provider, account) — the unit the sell switch, the
//! daily cap and the usage counters are all keyed on.
//!
//! The manifest is derived from the SQLite `tools` row, never the source of
//! truth; it exists so the isolation boundary is visible on disk. It is written
//! in exactly one place — [`resync`], driven by `commands::accounts_changed` —
//! which rewrites the whole directory from the table, so the two cannot
//! disagree. Do not add a second writer: the previous arrangement mirrored one
//! account at a time from whichever call site remembered to, and the ones that
//! forgot are what made "the file says selling, the table says off" possible.

use anyhow::{Context, Result};
use asale_client_core::store::LocalStore;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One account's on-disk manifest. Secrets live in the encrypted store; the two
/// `*_ref` fields are the keys that name them there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthManifest {
    pub provider: String,
    pub account_id: String,
    pub plan: Option<String>,
    /// `oauth` — asale ran its own browser login and exclusively owns this
    /// token. `import` — copied from a locally installed CLI, so the upstream
    /// refresh token is shared with that CLI and a refresh by either side can
    /// rotate the other out.
    pub origin: String,
    /// Where the credential in use came from: `keychain:<service>`, a file
    /// path, or `oauth`.
    pub source: String,
    /// Every local store found holding *this same* subscription account. One
    /// account is one manifest however many stores hold its token — this field
    /// is what makes the merge auditable on disk. Older manifests, written
    /// before the field existed, read back as empty.
    #[serde(default)]
    pub sources: Vec<String>,
    pub token_ref: String,
    pub refresh_ref: String,
    pub expires_at: Option<i64>,
    pub sell_enabled: bool,
    pub sell_daily_limit: i64,
    pub updated_at: i64,
}

/// `~/.asale/auths` — asale's own credential directory. Deliberately not any
/// vendor CLI's directory: nothing the sell side does may land in `~/.claude`.
pub fn auth_dir() -> PathBuf {
    PathBuf::from(crate::state::data_dir()).join("auths")
}

/// Characters that are unsafe in a file name, replaced with `_` (mirrors
/// CLIProxyAPI's `sanitizeCooldownFileName`).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

/// Short stable digest of the account id, so two accounts whose sanitized names
/// collide still get distinct files.
fn hash8(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// `<provider>-<hash8>-<account>[-<plan>].json`.
pub fn file_name(provider: &str, account_id: &str, plan: Option<&str>) -> String {
    let base = format!("{}-{}-{}", sanitize(provider), hash8(account_id), sanitize(account_id));
    match plan.map(str::trim).filter(|p| !p.is_empty()) {
        Some(plan) => format!("{base}-{}.json", sanitize(plan)),
        None => format!("{base}.json"),
    }
}

fn path_for(m: &AuthManifest) -> PathBuf {
    auth_dir().join(file_name(&m.provider, &m.account_id, m.plan.as_deref()))
}

/// Write (or replace) one account's manifest, atomically and owner-only.
/// A plan change renames the file, so drop any stale sibling for this account
/// first — otherwise the directory accumulates ghosts of old plan labels.
pub fn save(m: &AuthManifest) -> Result<PathBuf> {
    let dir = auth_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let target = path_for(m);
    for stale in files_for(&m.provider, &m.account_id) {
        if stale != target {
            let _ = std::fs::remove_file(stale);
        }
    }
    let body = serde_json::to_string_pretty(m)?;
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), file_name(&m.provider, &m.account_id, None)));
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &target).with_context(|| format!("rename into {}", target.display()))?;
    Ok(target)
}

/// Every manifest file belonging to this account, whatever plan label it was
/// last written with.
fn files_for(provider: &str, account_id: &str) -> Vec<PathBuf> {
    let prefix = format!("{}-{}-", sanitize(provider), hash8(account_id));
    let Ok(entries) = std::fs::read_dir(auth_dir()) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".json"))
        })
        .collect()
}

/// Drop an account's manifest(s) — called when the account is removed.
pub fn delete(provider: &str, account_id: &str) {
    for p in files_for(provider, account_id) {
        let _ = std::fs::remove_file(p);
    }
}

/// Rewrite the whole directory from the `tools` table: one manifest per account
/// that exists, and no file for one that does not.
///
/// This is the *only* writer. Each account used to be mirrored individually, at
/// whichever call site happened to remember — two of the five places that
/// change a `tools` row did, three did not — so an imported account had no
/// manifest until something unrelated touched it, and a sell switch or daily
/// cap edited afterwards left the file describing a state that was no longer
/// true. Deriving the whole set from the table at every change means the
/// directory cannot disagree with the database, whatever the caller forgot.
///
/// Best-effort by design: the manifest is an inspection artifact, never a
/// source of truth, so a write failure is logged and the command carries on.
pub async fn resync(store: &LocalStore) -> Vec<AuthManifest> {
    let Ok(tools) = store.list_tools().await else { return Vec::new() };
    // A CLI that is buying is not a supplier: its synced account has no manifest
    // while that is true, matching what `publisher::rebuild_pool` offers and
    // what the sell page shows.
    let buying = crate::tool_config::buying_set(
        store,
        tools.iter().filter(|t| t.origin.as_deref() == Some("import")).map(|t| t.provider.as_str()),
    )
    .await;
    let mut manifests = Vec::with_capacity(tools.len());
    for t in &tools {
        if t.origin.as_deref() == Some("import") && buying.contains(&t.provider) {
            continue;
        }
        let plan = store
            .get_setting(&format!("plan:{}:{}", t.provider, t.account_id))
            .await
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| t.plan.clone());
        let expires_at = store
            .get_setting(&crate::publisher::exp_key(&t.provider, &t.account_id))
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
        manifests.push(AuthManifest {
            provider: t.provider.clone(),
            account_id: t.account_id.clone(),
            plan,
            origin: t.origin.clone().unwrap_or_else(|| "import".into()),
            source: t.source.clone().unwrap_or_default(),
            sources: t.sources.clone(),
            token_ref: crate::keychain::token_ref(&t.provider, &t.account_id),
            refresh_ref: crate::keychain::refresh_ref(&t.provider, &t.account_id),
            expires_at,
            sell_enabled: t.sell_enabled,
            sell_daily_limit: t.sell_daily_limit,
            updated_at: now_secs(),
        });
    }

    let written = manifests.clone();
    let _ = tokio::task::spawn_blocking(move || write_all(&written)).await;
    manifests
}

/// Blocking half of [`resync`]: save each manifest, then remove any file that
/// no longer belongs to a live account.
fn write_all(manifests: &[AuthManifest]) {
    let mut keep = Vec::with_capacity(manifests.len());
    for m in manifests {
        match save(m) {
            Ok(p) => keep.push(p),
            Err(e) => tracing::warn!(
                provider = %m.provider,
                account_id = %m.account_id,
                "auth manifest write failed: {e}"
            ),
        }
    }
    let Ok(entries) = std::fs::read_dir(auth_dir()) else { return };
    for p in entries.flatten().map(|e| e.path()) {
        let is_manifest = p.extension().is_some_and(|x| x == "json");
        if is_manifest && !keep.contains(&p) {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Read every manifest in the directory (skipping unreadable/invalid files).
pub fn list() -> Vec<AuthManifest> {
    let Ok(entries) = std::fs::read_dir(auth_dir()) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|s| serde_json::from_str::<AuthManifest>(&s).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_dir<T>(f: impl FnOnce() -> T) -> T {
        crate::testenv::with_temp_data_dir("auths-test", f)
    }

    /// A `$HOME` with no vendor CLI in it.
    ///
    /// `with_temp_dir` moves `ASALE_DATA_DIR`, which is where the manifests go —
    /// but not `$HOME`, which is where [`crate::tool_config::points_at_proxy`]
    /// looks. Anything exercising [`resync`] therefore read the *developer's*
    /// real `~/.codex/config.toml`, and on any machine set up to buy through the
    /// market (the normal state for anyone testing the buy side) that file names
    /// our proxy — so an imported account was correctly excluded from the
    /// directory and the assertion below failed for a reason that had nothing to
    /// do with the code under test. Green on CI, red on the machine of everyone
    /// who had actually used the feature.
    ///
    /// Locked, and locked *before* `with_temp_dir`: `$HOME` is process-global.
    /// The order matters — no test takes the data-dir lock first and this one
    /// second, and that is what keeps the two from deadlocking.
    fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::codex_catalog::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        let tmp = std::env::temp_dir().join(format!("asale-auths-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);
        let out = f();
        match prev {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
        out
    }

    fn manifest(account: &str, plan: Option<&str>) -> AuthManifest {
        AuthManifest {
            provider: "claude".into(),
            account_id: account.into(),
            plan: plan.map(String::from),
            origin: "oauth".into(),
            source: "oauth".into(),
            sources: vec!["oauth".into()],
            token_ref: format!("claude:{account}"),
            refresh_ref: format!("claude:{account}:refresh"),
            expires_at: Some(123),
            sell_enabled: true,
            sell_daily_limit: 0,
            updated_at: 1,
        }
    }

    #[test]
    fn two_accounts_of_one_provider_get_separate_files() {
        with_temp_dir(|| {
            let a = save(&manifest("me@x.com", Some("max_20x"))).unwrap();
            let b = save(&manifest("alt@x.com", Some("max_20x"))).unwrap();
            assert_ne!(a, b, "each account owns a distinct file");
            assert_eq!(list().len(), 2);
            // The account id must not leak into a path outside the auth dir.
            assert_eq!(a.parent().unwrap(), auth_dir());
        });
    }

    #[test]
    fn resaving_with_a_new_plan_does_not_leave_a_ghost() {
        with_temp_dir(|| {
            save(&manifest("me@x.com", Some("pro"))).unwrap();
            save(&manifest("me@x.com", Some("max_20x"))).unwrap();
            let all = list();
            assert_eq!(all.len(), 1, "old plan file replaced, not accumulated");
            assert_eq!(all[0].plan.as_deref(), Some("max_20x"));
        });
    }

    /// The property the per-call-site mirroring could not hold: whatever the
    /// caller does to the `tools` table, the directory matches it afterwards.
    #[test]
    fn resync_makes_the_directory_match_the_table() {
        with_temp_home(|| with_temp_dir(|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let store = LocalStore::open_memory().await.unwrap();
                store
                    .upsert_tool("claude", "me@x.com", "ref", &["oauth"], "oauth")
                    .await
                    .unwrap();
                store
                    .upsert_tool("codex", "me@y.com", "ref2", &["import"], "import")
                    .await
                    .unwrap();
                resync(&store).await;
                assert_eq!(list().len(), 2);

                // A sell-switch edit used to leave the manifest describing the
                // old value, because nothing re-mirrored it.
                store.set_tool_sell("claude", "me@x.com", true, 500).await.unwrap();
                resync(&store).await;
                let claude = list().into_iter().find(|m| m.provider == "claude").unwrap();
                assert!(claude.sell_enabled);
                assert_eq!(claude.sell_daily_limit, 500);

                // A removed account leaves no file behind.
                store.delete_tool("codex", "me@y.com").await.unwrap();
                resync(&store).await;
                let left = list();
                assert_eq!(left.len(), 1, "stale manifest not cleaned up: {left:?}");
                assert_eq!(left[0].provider, "claude");
            });
        }))
    }

    #[test]
    fn hostile_account_ids_stay_inside_the_auth_dir() {
        with_temp_dir(|| {
            let p = save(&manifest("../../etc/passwd", None)).unwrap();
            assert_eq!(p.parent().unwrap(), auth_dir(), "path traversal sanitized away");
            delete("claude", "../../etc/passwd");
            assert!(list().is_empty());
        });
    }
}
