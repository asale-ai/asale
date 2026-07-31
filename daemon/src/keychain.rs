//! Encrypted on-disk secret store (spec §10 client).
//!
//! Credentials and the device seed used to live in the OS keychain; this module
//! keeps the same API but now persists everything to an encrypted file under the
//! app data dir (`~/.asale`), so we never touch the OS keychain and never prompt
//! for access. The SQLite store still holds only references, not secret values.
//!
//! Layout:
//!   - `~/.asale/secret.key`  — a random 32-byte ChaCha20-Poly1305 key (0600).
//!   - `~/.asale/secrets.enc` — base64(nonce ‖ ciphertext) of the JSON secret
//!                              map `{account: secret}` (0600).
//!
//! The whole map is (de)serialized on each call — secrets are few and small, and
//! a process-wide lock serializes reads/writes so concurrent proxy/publisher/UI
//! tasks can't race on the file. Secret values are never logged.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Serializes all file access so concurrent set/get/delete can't corrupt the
/// on-disk map (the store is read-modify-write). The guarded unit carries no
/// invariant, so a poisoned lock (a prior panic mid-op) is safe to recover.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The app data dir — the SQLite store's own, not a second opinion about where
/// it is. Resolving "home" separately here once put the secrets beside the
/// working directory while the store sat under the user's profile, which reads
/// as "this account has no token" rather than as a missing file.
fn data_dir() -> PathBuf {
    PathBuf::from(crate::state::data_dir())
}

fn key_path() -> PathBuf {
    data_dir().join("secret.key")
}

fn store_path() -> PathBuf {
    data_dir().join("secrets.enc")
}

/// Restrict a freshly written secret file to owner-only (0600) on Unix.
#[cfg(unix)]
fn lock_down(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn lock_down(_path: &std::path::Path) {}

/// Load the encryption key, generating and persisting a random one on first use.
fn load_or_create_key() -> Result<Key> {
    let path = key_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            return Ok(*Key::from_slice(&bytes));
        }
    }
    let dir = data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut raw = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    lock_down(&path);
    Ok(*Key::from_slice(&raw))
}

fn cipher() -> Result<ChaCha20Poly1305> {
    let key = load_or_create_key()?;
    Ok(ChaCha20Poly1305::new(&key))
}

/// Decrypt and parse the secret map (empty if the file is missing).
fn read_map() -> Result<BTreeMap<String, String>> {
    let raw = match std::fs::read_to_string(store_path()) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).context("read secrets.enc"),
    };
    let blob = B64.decode(raw.trim()).context("secrets.enc not valid base64")?;
    if blob.len() < 12 {
        anyhow::bail!("secrets.enc truncated");
    }
    let (nonce, ct) = blob.split_at(12);
    let plain = cipher()?
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow::anyhow!("secrets.enc decryption failed (wrong key or tampered)"))?;
    Ok(serde_json::from_slice(&plain).unwrap_or_default())
}

/// Encrypt and atomically write the secret map.
fn write_map(map: &BTreeMap<String, String>) -> Result<()> {
    let plain = serde_json::to_vec(map)?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher()?
        .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
        .map_err(|_| anyhow::anyhow!("secret encryption failed"))?;
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ct);
    let body = B64.encode(&blob);

    let dir = data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let target = store_path();
    let tmp = dir.join(format!("secrets.enc.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    lock_down(&tmp);
    std::fs::rename(&tmp, &target).with_context(|| format!("rename into {}", target.display()))?;
    Ok(())
}

/// Store a secret under `account` (e.g. "device_seed" or "claude:user@x").
pub fn set(account: &str, secret: &str) -> Result<()> {
    let _g = lock();
    let mut map = read_map()?;
    map.insert(account.to_string(), secret.to_string());
    write_map(&map)
}

/// Read a secret; returns None if absent.
pub fn get(account: &str) -> Result<Option<String>> {
    let _g = lock();
    Ok(read_map()?.get(account).cloned())
}

/// List stored secret keys (names only, never values) — for diagnostics
/// (the `limits_probe` example prints these to explain a missing token).
pub fn keys() -> Result<Vec<String>> {
    let _g = lock();
    Ok(read_map()?.keys().cloned().collect())
}

/// Delete a secret (best-effort — a missing entry is not an error).
pub fn delete(account: &str) -> Result<()> {
    let _g = lock();
    let mut map = read_map()?;
    if map.remove(account).is_some() {
        write_map(&map)?;
    }
    Ok(())
}

/// Store account name for a provider access token.
pub fn token_ref(provider: &str, account_id: &str) -> String {
    format!("{provider}:{account_id}")
}

/// Store account name for a provider refresh token.
pub fn refresh_ref(provider: &str, account_id: &str) -> String {
    format!("{provider}:{account_id}:refresh")
}

pub const DEVICE_SEED: &str = "device_seed";

#[cfg(test)]
mod tests {
    use super::*;

    // The store reads $ASALE_DATA_DIR (a process-global env var); the shared
    // helper serializes every test that repoints it, across all modules.
    fn with_temp_dir<T>(f: impl FnOnce() -> T) -> T {
        crate::testenv::with_temp_data_dir("kc-test", f)
    }

    #[test]
    fn set_get_delete_roundtrip_is_encrypted() {
        with_temp_dir(|| {
            assert_eq!(get("missing").unwrap(), None);

            set("access_token", "sk-secret-123").unwrap();
            set(&token_ref("claude", "user@x"), "tok-abc").unwrap();
            assert_eq!(get("access_token").unwrap().as_deref(), Some("sk-secret-123"));
            assert_eq!(get(&token_ref("claude", "user@x")).unwrap().as_deref(), Some("tok-abc"));

            // The plaintext secret must not appear on disk.
            let on_disk = std::fs::read_to_string(store_path()).unwrap();
            assert!(!on_disk.contains("sk-secret-123"), "secret stored in plaintext!");

            delete("access_token").unwrap();
            assert_eq!(get("access_token").unwrap(), None);
            assert_eq!(get(&token_ref("claude", "user@x")).unwrap().as_deref(), Some("tok-abc"));
        });
    }

    #[test]
    fn survives_reopen_with_persisted_key() {
        with_temp_dir(|| {
            set(DEVICE_SEED, "seed-xyz").unwrap();
            // Simulate a fresh process: read_map re-loads the key from disk.
            assert_eq!(get(DEVICE_SEED).unwrap().as_deref(), Some("seed-xyz"));
        });
    }
}
