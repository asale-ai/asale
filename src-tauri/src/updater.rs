//! Desktop updates prepared while the seller is still running.
//!
//! The native timer checks every ten minutes. User installs stage both binaries
//! and the desktop app before handing over to a supervisor. Protected macOS
//! installations request system authorization only after a user clicks install.
//! Windows uses the published portable executable; Linux system packages are
//! installed by a preauthorized system task before the desktop restarts.
//! A permission, download or preparation failure never exits the current app.

use std::path::PathBuf;
use std::process::Command;

use tauri::AppHandle;
#[cfg(target_os="linux")]
use tauri::Manager;

const INSTALL_SH: &str = "https://asale.ai/dl/install.sh";
const INSTALL_PS1: &str = "https://asale.ai/dl/install.ps1";
/// What the installers read to decide whether they have anything to do. Asking
/// the same file means the app and the installer can never disagree about which
/// release is current — and it is a plain document on asale.ai, so the check
/// works on a machine the signed update feed has never been set up for.
const MANIFEST: &str = "https://asale.ai/dl/manifest.json";

/// The published release, as much of it as anyone here needs.
#[derive(serde::Serialize)]
pub struct Release {
    pub version: String,
    /// The release page, for "what changed?".
    pub page: String,
}

/// One downloadable file in a release.
#[derive(Debug, Clone, serde::Deserialize)]
struct Asset {
    id: String,
    name: String,
    url: String,
    /// The other address for the same bytes. The manifest hands both over
    /// already sorted for whoever is asking, so this is simply "the one to try
    /// second" rather than a named host.
    #[serde(default)]
    mirror: String,
    /// Bytes, per the manifest. `0` when it does not say.
    #[serde(default)]
    size: u64,
    /// Hex SHA-256 of the file, per the manifest. Empty when it does not say —
    /// then the size is all there is to go on, and that is logged (S6).
    #[serde(default)]
    sha256: String,
}

/// Hex SHA-256 of a file on disk.
fn file_sha256(path: &std::path::Path) -> Result<String, String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| format!("could not hash {}: {e}", path.display()))?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// S6: the file at `dest` is what the manifest says it is — or it is deleted.
///
/// The installer runs whatever sits in this cache as root, and the cache is
/// writable by every process of this user; the byte count alone is what a
/// same-length swap defeats. A manifest with no hash is let through with a
/// warning while the feed catches up.
fn verify_asset(asset: &Asset, dest: &std::path::Path) -> Result<(), String> {
    if asset.sha256.is_empty() {
        tracing::warn!("manifest carries no sha256 for {}; installing on size alone", asset.name);
        return Ok(());
    }
    let got = file_sha256(dest)?;
    if !got.eq_ignore_ascii_case(asset.sha256.trim()) {
        let _ = std::fs::remove_file(dest);
        return Err(format!("{} does not match the manifest's sha256 — removed", asset.name));
    }
    Ok(())
}

/// How far along the download is, as the window renders it.
///
/// Cumulative across the whole set rather than per file: the user pressed one
/// button and is watching one bar, and "83% of the second of two files" is a
/// progress report that goes backwards.
#[derive(Clone, serde::Serialize)]
pub struct Progress {
    /// The file being fetched right now, for the label under the bar.
    pub file: String,
    pub received: u64,
    /// `0` when the manifest carried no sizes — the bar has to fall back to a
    /// byte count, because a percentage of an unknown total is a lie.
    pub total: u64,
}

/// Event name the window listens on. One channel for the whole download; the
/// payload says which file.
pub const PROGRESS_EVENT: &str = "update://progress";

/// Ask asale.ai what the current release is.
///
/// From Rust rather than from the webview: the manifest sends no CORS header,
/// so `fetch` from a `tauri://` origin never sees the response — and this way
/// the call goes through the user's configured proxy like every other outbound
/// request, instead of whatever the webview happens to be doing.
pub async fn latest_release() -> Result<Release, String> {
    let resp = asale_client_core::http::plain()
        .get(MANIFEST)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("could not reach {MANIFEST}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{MANIFEST} answered {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("{MANIFEST} is not JSON: {e}"))?;
    let version = body.get("version").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
    if version.is_empty() {
        return Err(format!("{MANIFEST} carries no version"));
    }
    Ok(Release {
        version,
        page: body.get("page").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
    })
}

/// The assets in the manifest, or an empty list when it does not carry any.
async fn assets() -> Result<(String, Vec<Asset>), String> {
    let resp = asale_client_core::http::plain()
        .get(MANIFEST)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("could not reach {MANIFEST}: {e}"))?;
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("{MANIFEST} is not JSON: {e}"))?;
    let version = body.get("version").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
    let builds = body.get("builds").cloned().unwrap_or_default();
    let builds: Vec<Asset> = serde_json::from_value(builds).unwrap_or_default();
    Ok((version, builds))
}

/// Which of them this machine needs, in the order they should be fetched.
///
/// The command line first, deliberately: it is the smaller half everywhere, so
/// the bar moves early — and if the user cancels by quitting the window
/// mid-download, the half that was completed is the cheap one to have kept.
///
/// The desktop asset is the installer's own choice mirrored back: on Linux it
/// takes the `.deb` where `dpkg` exists and the AppImage otherwise. Guessing
/// wrong is not a failure, only a miss — the installer downloads what it wanted
/// and the cached file goes unused.
fn wanted_ids() -> [&'static str; 2] {
    if cfg!(target_os = "macos") {
        ["cli-mac", "mac"]
    } else if cfg!(windows) {
        ["cli-windows", "windows-portable"]
    } else if std::env::var_os("APPIMAGE").is_none() && std::path::Path::new("/usr/bin/dpkg").exists() {
        ["cli-linux", "linux-deb"]
    } else {
        ["cli-linux", "linux-appimage"]
    }
}

/// Where a downloaded release waits for the installer.
///
/// Under the release's own version, so a half-finished download of one version
/// can never be handed to an installer fetching another — and so the whole thing
/// is one directory to delete.
fn cache_dir(version: &str) -> PathBuf {
    work_dir().join("updates").join(version)
}

/// Fetch this platform's half of `version` into [`cache_dir`], reporting
/// progress to the window as it goes. Returns the directory.
///
/// This is the part that used to be invisible. The installer downloaded
/// everything itself, elevated, after the app had already quit — so "update"
/// meant the window vanishing for a minute or two with nothing to look at, on a
/// connection whose speed nobody had told the user about. Doing it here, first,
/// costs nothing (the installer copies from the cache instead of fetching) and
/// buys the one thing that was missing: the user can see it working, and the app
/// only closes once the bytes are safely on disk.
pub async fn download(app: &AppHandle) -> Result<PathBuf, String> {
    use tauri::Emitter;
    let (version, assets) = assets().await?;
    for id in wanted_ids() {
        let asset = assets.iter().find(|a| a.id == id).ok_or_else(|| format!("release is missing {id}"))?;
        if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(format!("release asset {id} is missing a valid SHA-256")); }
    }
    let app = app.clone();
    fetch_release(&version, &assets, move |p| {
        let _ = app.emit(PROGRESS_EVENT, p);
    })
    .await
}

/// The download itself, with reporting left to the caller.
///
/// Split from [`download`] so it can be tested: emitting needs a running Tauri
/// application, and the parts worth checking — that the second address is tried,
/// that a short file is rejected, that progress adds up across both files — have
/// nothing to do with Tauri.
async fn fetch_release(
    version: &str,
    assets: &[Asset],
    report: impl Fn(Progress) + Send + Sync,
) -> Result<PathBuf, String> {
    if version.is_empty() {
        return Err(format!("{MANIFEST} carries no version"));
    }
    let wanted: Vec<Asset> = wanted_ids()
        .iter()
        .filter_map(|id| assets.iter().find(|a| a.id == *id).cloned())
        .collect();
    if wanted.len() != 2 {
        return Err(format!("release {version} publishes nothing for this platform"));
    }

    let dir = cache_dir(version);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    // Anything cached for another version is now dead weight — an app that
    // updated three times would otherwise be sitting on three releases.
    sweep_old_caches(version);

    let total: u64 = wanted.iter().map(|a| a.size).sum();
    let mut done: u64 = 0;
    for asset in &wanted {
        let dest = dir.join(&asset.name);
        // Already here and the right size: a retry after a failed install, or
        // the user pressing update twice. Re-fetching 90 MB to learn that would
        // be the slowest possible way to do nothing.
        if asset.size > 0
            && std::fs::metadata(&dest).map(|m| m.len()).ok() == Some(asset.size)
            // S6: a cached file is reused only when its hash still matches;
            // a mismatch deletes it and falls through to a fresh download.
            && verify_asset(asset, &dest).is_ok()
        {
            done += asset.size;
            report(Progress { file: asset.name.clone(), received: done, total });
            continue;
        }
        fetch(&report, asset, &dest, done, total).await?;
        verify_asset(asset, &dest)?;
        done += match asset.size {
            0 => std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
            n => n,
        };
    }
    tracing::info!("update {version} downloaded to {}", dir.display());
    Ok(dir)
}

/// One file, streamed to disk, with the second address tried when the first
/// does not answer — the same two-addresses-for-the-same-bytes rule the
/// installer follows, for the same reason (one of them is unreachable from some
/// networks, and which one that is depends on where the user is).
async fn fetch(
    report: &(impl Fn(Progress) + Send + Sync),
    asset: &Asset,
    dest: &std::path::Path,
    done: u64,
    total: u64,
) -> Result<(), String> {
    let mut last = String::new();
    for url in [asset.url.as_str(), asset.mirror.as_str()] {
        if url.is_empty() {
            continue;
        }
        match stream_to_file(report, asset, url, dest, done, total).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!("{url} did not deliver {}: {e}", asset.name);
                last = e;
                let _ = std::fs::remove_file(dest);
            }
        }
    }
    Err(format!("could not download {}: {last}", asset.name))
}

async fn stream_to_file(
    report: &(impl Fn(Progress) + Send + Sync),
    asset: &Asset,
    url: &str,
    dest: &std::path::Path,
    done: u64,
    total: u64,
) -> Result<(), String> {
    use std::io::Write;

    let mut resp = asale_client_core::http::plain()
        .get(url)
        // No overall deadline: this is up to ninety megabytes on whatever
        // connection the user has. The read timeout below is what catches a
        // download that has actually stalled, which a total timeout cannot tell
        // apart from one that is merely slow.
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{} answered {}", url, resp.status()));
    }
    // Written next to the real name and renamed at the end: a half-file left by
    // a crash must never be picked up as a complete download by the size check
    // in `download`.
    let part = dest.with_extension("part");
    let mut file = std::fs::File::create(&part).map_err(|e| format!("could not write {}: {e}", part.display()))?;
    let mut received = 0u64;
    let mut last_emit = std::time::Instant::now();
    loop {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(45), resp.chunk())
            .await
            .map_err(|_| "the download stalled".to_string())?
            .map_err(|e| e.to_string())?;
        let Some(chunk) = chunk else { break };
        file.write_all(&chunk).map_err(|e| format!("could not write {}: {e}", part.display()))?;
        received += chunk.len() as u64;
        // Throttled: a 90 MB file arrives in thousands of chunks, and an event
        // per chunk is a webview redrawing a progress bar faster than a screen
        // can show it.
        if last_emit.elapsed() >= std::time::Duration::from_millis(120) {
            last_emit = std::time::Instant::now();
            report(Progress { file: asset.name.clone(), received: done + received, total });
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    if asset.size > 0 && received != asset.size {
        return Err(format!("got {received} bytes, expected {} — truncated or intercepted", asset.size));
    }
    std::fs::rename(&part, dest).map_err(|e| format!("could not finish {}: {e}", dest.display()))?;
    report(Progress { file: asset.name.clone(), received: done + received, total });
    Ok(())
}

/// Delete every cached release except `keep`.
fn sweep_old_caches(keep: &str) {
    let root = work_dir().join("updates");
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy() != keep {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The most recently written cached release, if one is sitting there ready to
/// install.
///
/// Read at helper-writing time rather than remembered from the download, so a
/// download that finished in a previous run of the app still counts — and so
/// nothing breaks if the two calls arrive out of order.
fn ready_cache() -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(work_dir().join("updates")).ok()?.flatten() {
        let p = e.path();
        // Non-empty only: an empty directory is a download that never started,
        // and pointing the installer at one would just make it fetch everything
        // itself, which is what it does with no cache at all.
        if !p.is_dir() || !std::fs::read_dir(&p).map(|mut d| d.next().is_some()).unwrap_or(false) {
            continue;
        }
        let at = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(seen, _)| at >= *seen) {
            best = Some((at, p));
        }
    }
    best.map(|(_, p)| p)
}

/// The command this would run, for the Settings page to show before it is run.
/// Nobody should have to take "an installer" on trust — and it is built from
/// the same two constants the helper is, so what is shown cannot drift from
/// what happens.
pub fn install_command() -> String {
    if cfg!(windows) {
        format!("irm {INSTALL_PS1} | iex")
    } else {
        format!("curl -fsSL {INSTALL_SH} | sh")
    }
}

/// Persist the handover before exiting. An old build reopened by rollback must
/// not automatically attempt the same release again, even after another launch.
#[derive(serde::Serialize, serde::Deserialize)]
struct UpdateAttempt {
    version: String,
    error: String,
}

fn attempt_path() -> PathBuf { work_dir().join("desktop-update-attempt.json") }

fn save_attempt(version: &str, error: &str) -> Result<(), String> {
    let bytes = serde_json::to_vec(&UpdateAttempt { version: version.into(), error: error.into() }).map_err(|e| e.to_string())?;
    let path = attempt_path();
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    if path.exists() { std::fs::remove_file(&path).map_err(|e| e.to_string())?; }
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

fn read_attempt() -> Result<Option<UpdateAttempt>, String> {
    match std::fs::read(attempt_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("Could not read the previous update result: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Could not read the previous update result: {e}")),
    }
}

fn reconcile_attempt(current: &str) -> Result<String, String> {
    let Some(attempt) = read_attempt()? else { return Ok(String::new()); };
    if !newer(&attempt.version, current) {
        std::fs::remove_file(attempt_path()).map_err(|e| e.to_string())?;
        return Ok(String::new());
    }
    let error = if attempt.error.is_empty() {
        format!("Update to {} did not complete; still running {current}. Automatic retries for this release are paused. Click Install update to retry.", attempt.version)
    } else { attempt.error };
    save_attempt(&attempt.version, &error)?;
    Ok(error)
}

fn automatic_attempt_allowed(version: &str) -> Result<bool, String> {
    Ok(read_attempt()?.is_none_or(|attempt| attempt.version != version))
}

struct PreparedUpdate {
    script: PathBuf,
    version: String,
    // A fixed, quoted transaction, never a path to a user-writable root script.
    authorization: Option<String>,
}

/// Write the helper, start it, and quit.
pub fn run(app: &AppHandle) -> Result<(), String> {
    run_with_mode(app, false)
}

fn run_with_mode(app: &AppHandle, automatic: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("APPIMAGE").is_none() {
        let output = Command::new("dpkg-query").args(["-W", "-f=${db:Status-Status}|${Version}", "asale"]).output().map_err(|e| e.to_string())?;
        if output.status.success() {
            let installed = String::from_utf8_lossy(&output.stdout);
            if let Some((status, version)) = installed.trim().split_once('|') {
                if status == "installed" && newer(version, env!("CARGO_PKG_VERSION")) {
                    let target = tauri::process::current_binary(&app.env()).map_err(|e|e.to_string())?;
                    let script = work_dir().join("package-restart.sh");
                    let pid = std::process::id();
                    std::fs::write(&script, format!("#!/bin/sh\nn=0\nwhile kill -0 {pid} 2>/dev/null; do\n [ \"$n\" -lt 120 ] || exit 1\n sleep 0.5; n=$((n+1))\ndone\n{} >/dev/null 2>&1 &\n",sq(&target.to_string_lossy()))).map_err(|e|e.to_string())?;
                    spawn_detached(&script)?;
                    let handle=app.clone();
                    std::thread::spawn(move || { std::thread::sleep(std::time::Duration::from_millis(600)); handle.exit(0); });
                    return Ok(());
                }
            }
        }
        return Err("Waiting for the authorized system package updater. Enable it once with: sudo asale autoupdate enable --system-package --data-dir <your Asale data directory>. Selling continues.".into());
    }

    static APPLYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if APPLYING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("An update is already being applied".into());
    }
    let prepared = prepare_safe_helper(automatic);
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(e) => { APPLYING.store(false, std::sync::atomic::Ordering::SeqCst); return Err(e); }
    };
    let version = &prepared.version;
    if let Err(e) = save_attempt(version, "") {
        APPLYING.store(false, std::sync::atomic::Ordering::SeqCst);
        return Err(e);
    }
    #[cfg(target_os = "macos")]
    if let Some(command) = prepared.authorization.as_deref() {
        if let Err(e) = authorize_macos(command) {
            let _ = save_attempt(version, &e);
            APPLYING.store(false, std::sync::atomic::Ordering::SeqCst);
            return Err(e);
        }
    }
    if let Err(e) = spawn_detached(&prepared.script) {
        let _ = save_attempt(version, &e);
        APPLYING.store(false, std::sync::atomic::Ordering::SeqCst);
        return Err(e);
    }
    mark_relaunch();
    tracing::info!("update helper started: {}", prepared.script.display());

    // Quit on a delay rather than here: the command has to return first, or the
    // webview never gets a frame to replace the button with "updating…" — and
    // the helper is polling for this pid, so a few hundred ms costs nothing.
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        handle.exit(0);
    });
    Ok(())
}

/// Native timer keeps checking even when the webview is hidden in the tray.
pub fn watch(app: AppHandle, shell: std::sync::Arc<crate::Shell>) {
    if cfg!(debug_assertions) { return; }
    tauri::async_runtime::spawn(async move {
        let recovered = reconcile_attempt(env!("CARGO_PKG_VERSION"));
        let http = asale_client_core::http::plain();
        let error = recovered.unwrap_or_else(|e| e);
        let _ = http.post(format!("{}/rpc/set_setting", shell.daemon_base))
            .header("X-Asale-Token", &shell.token)
            .json(&serde_json::json!({"key":"auto_update_error","value":error})).send().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            let http = asale_client_core::http::plain();
            let preference = http.post(format!("{}/rpc/get_setting", shell.daemon_base))
                .header("X-Asale-Token", &shell.token)
                .json(&serde_json::json!({"key":"auto_update"})).send().await;
            // An unreadable preference is not authorization to restart.
            if let Ok(response) = preference {
                if let Ok(value) = response.json::<serde_json::Value>().await {
                    if value.is_null() || value.as_str().is_some_and(|v| v != "0") {
                        if let Ok(release) = latest_release().await {
                            if newer(&release.version, env!("CARGO_PKG_VERSION")) && automatic_attempt_allowed(&release.version).unwrap_or(false) {
                                let result = match download(&app).await {
                                    Ok(_) => {
                                        let handle = app.clone();
                                        let result = tauri::async_runtime::spawn_blocking(move || run_with_mode(&handle, true)).await
                                            .unwrap_or_else(|e| Err(e.to_string()));
                                        if let Err(error) = &result {
                                            if let Err(e) = save_attempt(&release.version, error) {
                                                tracing::error!("could not persist failed update: {e}");
                                            }
                                        }
                                        result
                                    },
                                    Err(e) => Err(e),
                                };
                                let error = result.err().unwrap_or_default();
                                let _ = http.post(format!("{}/rpc/set_setting", shell.daemon_base))
                                    .header("X-Asale-Token", &shell.token)
                                    .json(&serde_json::json!({"key":"auto_update_error","value":error})).send().await;
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }
    });
}

fn newer(latest: &str, current: &str) -> bool {
    fn version(v: &str) -> Option<Vec<u64>> {
        v.trim_start_matches('v').split('-').next()?.split('.').map(|n| n.parse().ok()).collect()
    }
    match (version(latest), version(current)) {
        (Some(a), Some(b)) => a > b || (a == b && current.contains('-') && !latest.contains('-')),
        _ => false,
    }
}

/// Prepare the entire replacement while the seller is still running. Background
/// checks never request authorization; an explicit macOS install may use the
/// system authorization dialog after preparation succeeds.
#[cfg(unix)]
fn prepare_safe_helper(automatic: bool) -> Result<PreparedUpdate, String> {
    use std::os::unix::fs::PermissionsExt;
    let target = relaunch_target();
    let parent = target.parent().ok_or("invalid application path")?;
    if cfg!(target_os = "macos") && target.extension().and_then(|s| s.to_str()) != Some("app") {
        return Err("Automatic updates require an installed .app bundle".into());
    }
    if !cfg!(target_os = "macos") && std::env::var_os("APPIMAGE").is_none() {
        return Err("This package is managed by the system. Upgrade it with your package manager; selling continues.".into());
    }
    let cache = ready_cache().ok_or("Download the update first")?;
    let expected_version = cache.file_name().and_then(|n| n.to_str()).ok_or("invalid release version")?.to_string();
    let cli = Command::new("/bin/sh").args(["-c", "command -v asale"]).output().map_err(|e| e.to_string())?;
    let home = PathBuf::from(std::env::var("HOME").map_err(|e| e.to_string())?);
    let cli_path = PathBuf::from(String::from_utf8_lossy(&cli.stdout).trim());
    // Finder launches have a minimal PATH; don't leave an existing /usr/local
    // CLI on an old release merely because command -v cannot see it.
    let cli_dir = if cli.status.success() { cli_path.parent().ok_or("invalid CLI path")?.to_path_buf() }
        else { [home.join(".local/bin"), PathBuf::from("/usr/local/bin")].into_iter()
            .find(|dir| dir.join("asale").is_file()).unwrap_or_else(|| home.join(".local/bin")) };
    let needs_authorization = !writable(&target) || !writable(parent)
        || (cli_dir.exists() && !writable(&cli_dir))
        || ["asale", "asaled"].iter().any(|name| { let p = cli_dir.join(name); p.exists() && !writable(&p) });
    if needs_authorization && (automatic || !cfg!(target_os = "macos")) {
        return Err("This update needs system authorization. Automatic retries for this release are paused; click Install update in Settings. The current client keeps running.".into());
    }
    let attempt = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    // Protected installs are staged in the user's data directory. Authorization
    // is requested only once everything is downloaded, verified and prepared.
    let staged = if needs_authorization { work_dir().join(format!(".asale-update-{attempt}")) }
        else { parent.join(format!(".asale-update-{attempt}")) };
    std::fs::create_dir(&staged).map_err(|e| e.to_string())?;
    let files: Vec<PathBuf> = std::fs::read_dir(cache).map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path())).collect();
    let prepared = staged.join(target.file_name().ok_or("invalid application name")?);
    #[cfg(target_os = "macos")]
    {
        let dmg = files.iter().find(|p| p.extension().is_some_and(|e| e == "dmg")).ok_or("Missing desktop download")?;
        let mount = staged.join("mount");
        std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
        let mounted = Command::new("/usr/bin/hdiutil").args(["attach", "-nobrowse", "-quiet", "-mountpoint"]).arg(&mount).arg(dmg).status().map_err(|e| e.to_string())?;
        if !mounted.success() { return Err("Could not mount downloaded update; selling continues".into()); }
        let result = (|| {
            let app = std::fs::read_dir(&mount).map_err(|e| e.to_string())?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .find(|p| p.extension().is_some_and(|e| e == "app")).ok_or("Missing app in download")?;
            let status = Command::new("/usr/bin/ditto").arg(&app).arg(&prepared).status().map_err(|e| e.to_string())?;
            if !status.success() { return Err("Could not stage app; selling continues".into()); }
            let signed = Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&prepared).status().map_err(|e| e.to_string())?;
            if !signed.success() { return Err("Update signature verification failed; selling continues".into()); }
            let plist = Command::new("/usr/libexec/PlistBuddy").args(["-c", "Print :CFBundleShortVersionString"])
                .arg(prepared.join("Contents/Info.plist")).output().map_err(|e| e.to_string())?;
            if !plist.status.success() || String::from_utf8_lossy(&plist.stdout).trim() != expected_version {
                return Err("The downloaded app does not match the requested release; the current client keeps running".into());
            }
            Ok::<(), String>(())
        })();
        let _ = Command::new("/usr/bin/hdiutil").args(["detach", "-quiet"]).arg(&mount).status();
        result?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let image = files.iter().find(|p| p.extension().is_some_and(|e| e == "AppImage")).ok_or("Missing AppImage download")?;
        std::fs::copy(image, &prepared).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&prepared, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let cli_stage = if needs_authorization { staged.join("cli") } else {
        std::fs::create_dir_all(&cli_dir).map_err(|e| e.to_string())?;
        cli_dir.join(format!(".asale-update-{attempt}"))
    };
    std::fs::create_dir(&cli_stage).map_err(|e| e.to_string())?;
    let archive = files.iter().find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("asale-cli-") && n.to_string_lossy().ends_with(".tar.gz"))).ok_or("Missing CLI download")?;
    let extracted = Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&cli_stage).status().map_err(|e| e.to_string())?;
    if !extracted.success() { return Err("Could not stage CLI update; selling continues".into()); }
    for name in ["asale", "asaled"] {
        if !cli_stage.join(name).is_file() { return Err(format!("Missing staged {name}; selling continues")); }
    }
    let cli_dir = sq(&cli_dir.to_string_lossy());
    let cli_stage = sq(&cli_stage.to_string_lossy());
    let target = sq(&target.to_string_lossy());
    let prepared = sq(&prepared.to_string_lossy());
    let backup = sq(&staged.join("previous").to_string_lossy());
    let pid = std::process::id();
    let launch = if cfg!(target_os = "macos") { format!("/usr/bin/open {target}") } else { format!("{target} >/dev/null 2>&1 &") };
    let swap = swap_commands(&target, &prepared, &backup, &cli_dir, &cli_stage);
    let lease = format!("mkdir {cli_dir}/.asale-update-in-progress 2>/dev/null || exit 1\ncleanup() {{ rmdir {cli_dir}/.asale-update-in-progress; }}\ntrap cleanup EXIT\n");
    let authorization = needs_authorization.then(|| format!("PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH\nmkdir -p {cli_dir} || exit 1\n{lease}{swap}"));
    // For protected installs the authorized transaction completes before exit;
    // this helper only waits and reopens the app as the ordinary user.
    let transaction = if needs_authorization { ":".into() } else {
        format!("{lease}(\n{swap}\n)")
    };
    let body = unix_helper_body(pid, &transaction, &launch, &work_dir().join("update.log"));
    let helper = work_dir().join("safe-update-helper.sh");
    std::fs::write(&helper, body).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(PreparedUpdate { script: helper, version: expected_version, authorization })
}

#[cfg(unix)]
fn unix_helper_body(pid: u32, transaction: &str, launch: &str, log: &std::path::Path) -> String {
    format!("#!/bin/sh\nexec >>{} 2>&1\nn=0\nwhile kill -0 {pid} 2>/dev/null; do\n  [ \"$n\" -lt 120 ] || exit 1\n  sleep 0.5\n  n=$((n+1))\ndone\n(\n{transaction}\n)\n{launch}\n", sq(&log.to_string_lossy()))
}

#[cfg(unix)]
fn writable(path: &std::path::Path) -> bool {
    Command::new("/bin/test").arg("-w").arg(path).status().is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn authorize_macos(command: &str) -> Result<(), String> {
    let script = format!("do shell script {} with administrator privileges", applescript_string(command));
    let output = Command::new("/usr/bin/osascript").args(["-e", &script]).output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("Update authorization or installation did not complete. The current client keeps running. {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\"").replace('\n', "\\n").replace('\r', "\\r"))
}

#[cfg(unix)]
fn swap_commands(target: &str, prepared: &str, backup: &str, cli_dir: &str, cli_stage: &str) -> String {
    format!(r#"rollback() {{
  [ ! -e {backup} ] || {{ rm -rf {target}; mv {backup} {target}; }}
  for name in asale asaled; do
    [ ! -e {cli_stage}/$name.absent ] || rm -f {cli_dir}/$name
    [ ! -e {cli_stage}/$name.previous ] || {{ rm -f {cli_dir}/$name; mv {cli_stage}/$name.previous {cli_dir}/$name; }}
  done
}}
apply() {{
  for name in asale asaled; do
    if [ -e {cli_dir}/$name ]; then mv {cli_dir}/$name {cli_stage}/$name.previous || return 1; else touch {cli_stage}/$name.absent || return 1; fi
    mv {cli_stage}/$name {cli_dir}/$name || return 1
  done
  mv {target} {backup} || return 1
  mv {prepared} {target} || return 1
}}
if ! apply; then
  rollback
  exit 1
fi
"#)
}

#[cfg(windows)]
fn prepare_safe_helper(_automatic: bool) -> Result<PreparedUpdate, String> {
    let cache = ready_cache().ok_or("Download the update first")?;
    let version = cache.file_name().and_then(|n| n.to_str()).ok_or("invalid release version")?.to_string();
    prepare_windows_helper().map(|script| PreparedUpdate { script, version, authorization: None })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn prepare_windows_helper() -> Result<PathBuf, String> {
    let target = relaunch_target();
    let parent = target.parent().ok_or("invalid application path")?;
    let attempt = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e|e.to_string())?.as_nanos();
    let stage = parent.join(format!(".asale-update-{attempt}"));
    if std::fs::create_dir(&stage).is_err() {
        let task = Command::new("schtasks.exe").args(["/Run", "/TN", "AsaleAutomaticUpdate"]).status().map_err(|e|e.to_string())?;
        return Err(if task.success() { "Update delegated to the authorized background task; selling continues" } else { "Enable the system update task once in an administrator terminal: asale autoupdate enable --desktop-target <installed Asale.exe>. Selling continues" }.into());
    }
    let cache = ready_cache().ok_or("Download the update first")?;
    let expected_version = cache.file_name().and_then(|n|n.to_str()).ok_or("invalid release version")?.to_string();
    let embedded = crate::EMBEDDED_DAEMON.load(std::sync::atomic::Ordering::SeqCst);
    let port = asale_daemon::bound_addr().map(|a|a.port()).unwrap_or(9700);
    let files: Vec<PathBuf> = std::fs::read_dir(cache).map_err(|e|e.to_string())?.filter_map(|e|e.ok().map(|e|e.path())).collect();
    let app = files.iter().find(|p|p.file_name().is_some_and(|n|n.to_string_lossy().ends_with("-portable.exe"))).ok_or("Release is missing its silent Windows update executable")?;
    let prepared = stage.join("Asale.exe");
    std::fs::copy(app, &prepared).map_err(|e|e.to_string())?;
    let cli_dir = PathBuf::from(std::env::var("LOCALAPPDATA").map_err(|e|e.to_string())?).join("Asale/bin");
    std::fs::create_dir_all(&cli_dir).map_err(|e|e.to_string())?;
    let cli_stage = cli_dir.join(format!(".asale-update-{attempt}"));
    std::fs::create_dir(&cli_stage).map_err(|e|e.to_string())?;
    let archive = files.iter().find(|p|p.file_name().is_some_and(|n|n.to_string_lossy().starts_with("asale-cli-") && n.to_string_lossy().ends_with(".zip"))).ok_or("Missing CLI download")?;
    let result = Command::new("powershell.exe").args(["-NoProfile","-Command"]).arg(format!("$ErrorActionPreference='Stop'; Expand-Archive -LiteralPath {} -DestinationPath {}",pq(&archive.to_string_lossy()),pq(&cli_stage.to_string_lossy()))).status().map_err(|e|e.to_string())?;
    if !result.success() || !cli_stage.join("asale.exe").is_file() || !cli_stage.join("asaled.exe").is_file() { return Err("Could not prepare CLI update; selling continues".into()); }
    // An independently running daemon owns its own CLI update transaction.
    // Replacing its locked executable from the GUI would fail on Windows.
    let cli_names = if crate::EMBEDDED_DAEMON.load(std::sync::atomic::Ordering::SeqCst) { "@('asale.exe','asaled.exe')" } else { "@()" };
    let body = format!(r#"$ErrorActionPreference='Stop'
$oldPid={pid}; $target={target}; $prepared={prepared}; $backup={backup}
$cli={cli}; $stage={stage}
$lease=[IO.File]::Open((Join-Path $cli '.asale-update.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
try {{ Wait-Process -Id $oldPid -Timeout 60 -ErrorAction Stop }} catch {{ if(Get-Process -Id $oldPid -ErrorAction SilentlyContinue) {{ exit 1 }} }}
try {{
 foreach($n in {cli_names}) {{
  $current=Join-Path $cli $n
  if(Test-Path $current) {{ Move-Item $current (Join-Path $stage "$n.previous") -Force }}
  Move-Item (Join-Path $stage $n) $current -Force
 }}
 Move-Item $target $backup -Force
 Move-Item $prepared $target -Force
 $started=Start-Process -FilePath $target -PassThru
 if({embedded}) {{
  $ready=$false
  for($i=0;$i -lt 60;$i++) {{
   Start-Sleep -Seconds 1
   try {{ $health=Invoke-RestMethod 'http://127.0.0.1:{port}/healthz'; if($health.version -eq {expected_version}) {{ $ready=$true;break }} }} catch {{}}
  }}
  if(-not $ready) {{ throw 'updated desktop failed its health check' }}
 }}
}} catch {{
 if(Get-Variable started -ErrorAction SilentlyContinue) {{ Stop-Process -Id $started.Id -Force -ErrorAction SilentlyContinue }}
 if(Test-Path $backup) {{ Remove-Item $target -Force -ErrorAction SilentlyContinue; Move-Item $backup $target -Force }}
 foreach($n in {cli_names}) {{
  $old=Join-Path $stage "$n.previous"
  if(Test-Path $old) {{ Remove-Item (Join-Path $cli $n) -Force -ErrorAction SilentlyContinue; Move-Item $old (Join-Path $cli $n) -Force }}
 }}
 Start-Process -FilePath $target
}}
"#,embedded=if embedded { "$true" } else { "$false" }, expected_version=pq(&expected_version),pid=std::process::id(),target=pq(&target.to_string_lossy()),prepared=pq(&prepared.to_string_lossy()),backup=pq(&stage.join("previous.exe").to_string_lossy()),cli=pq(&cli_dir.to_string_lossy()),stage=pq(&cli_stage.to_string_lossy()));
    let helper = stage.join("supervisor.ps1"); std::fs::write(&helper,body).map_err(|e|e.to_string())?; Ok(helper)
}

/// The helper and its log live with the rest of asale's state, not in a temp
/// directory: a failed update is exactly the thing someone comes looking for a
/// day later, and `/tmp` will not have it.
fn work_dir() -> PathBuf {
    let dir = PathBuf::from(asale_daemon::state::data_dir());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Left behind by the build that quit for an update, picked up by the build
/// that replaces it. A file rather than a launch argument because the two ends
/// are different binaries started minutes apart by a script in between, and an
/// argument would have to survive `open`, `Start-Process` and the
/// single-instance plugin to get here.
fn relaunch_flag() -> PathBuf {
    work_dir().join("reopen-after-update")
}

fn mark_relaunch() {
    if let Err(e) = std::fs::write(relaunch_flag(), "") {
        // Not fatal: the helper still reopens the app, it just may come up
        // behind another window.
        tracing::warn!("could not record the post-update relaunch: {e}");
    }
}

/// Whether this launch is the one that follows an update — consumed on read, so
/// a flag stranded by a crash mid-install cannot raise the window on every
/// launch from then on.
pub fn take_relaunch_flag() -> bool {
    let path = relaunch_flag();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        return true;
    }
    false
}

/// The app to open once the installer is done.
///
/// Taken from this process rather than from a guess at the install location: an
/// installer replaces a bundle in place, so wherever this build was launched
/// from is where the new one will be. The AppImage is the exception — it is
/// mounted under `/tmp` while it runs and `$APPIMAGE` is the only pointer back
/// to the real file.
fn relaunch_target() -> PathBuf {
    if let Ok(img) = std::env::var("APPIMAGE") {
        if !img.trim().is_empty() {
            return PathBuf::from(img);
        }
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("asale"));
    if cfg!(target_os = "macos") {
        // …/Asale.app/Contents/MacOS/Asale → …/Asale.app, which is what `open`
        // understands; handing it the inner binary starts a second, dockless
        // copy of the process instead of launching the bundle.
        if let Some(app) = exe
            .ancestors()
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("app"))
        {
            return app.to_path_buf();
        }
    }
    exe
}

// ── the helper itself ───────────────────────────────────────────────────────

// ── starting it, properly detached ──────────────────────────────────────────

#[cfg(unix)]
fn spawn_detached(script: &std::path::Path) -> Result<(), String> {
    // `nohup … &` rather than a plain spawn: the helper has to outlive both this
    // process and the SIGHUP that reaches its process group when the app goes.
    // (`setsid` would be tidier but it is util-linux — macOS has no such binary.)
    let cmd = format!("nohup /bin/sh {} >/dev/null 2>&1 </dev/null &", sq(&script.to_string_lossy()));
    Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .spawn()
        .map_err(|e| format!("could not start the update helper: {e}"))?;
    Ok(())
}

#[cfg(windows)]
fn spawn_detached(script: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    /// Its own console, so the installer's progress is visible while the app is
    /// closed, and its own process group, so the app's exit does not take it.
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .creation_flags(CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map_err(|e| format!("could not start the update helper: {e}"))?;
    Ok(())
}

// ── quoting ─────────────────────────────────────────────────────────────────

/// POSIX single-quoting. Paths come from `$HOME` and from the install location,
/// neither of which this code chose — a space in either would otherwise split
/// the command into two.
#[cfg(unix)]
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double-quoting for a path that has to survive *two* layers: the AppleScript
/// string literal the `do shell script` command is written as, and the shell
/// that then runs that command.
///
/// The PowerShell spelling of the same thing: inside single quotes only the
/// quote itself is special, and it is escaped by doubling.
#[cfg_attr(not(windows), allow(dead_code))]
fn pq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(all(test, unix))]
mod tests {
    struct TestDataDir {
        path: PathBuf,
        previous: Option<std::ffi::OsString>,
    }
    impl TestDataDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("asale-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            let previous = std::env::var_os("ASALE_DATA_DIR");
            std::env::set_var("ASALE_DATA_DIR", &path);
            Self { path, previous }
        }
    }
    impl Drop for TestDataDir {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("ASALE_DATA_DIR", value),
                None => std::env::remove_var("ASALE_DATA_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn failed_or_interrupted_update_does_not_repeat_after_relaunch() {
        let _lock = DATA_DIR.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = TestDataDir::new("update-recovery");
        save_attempt("1.2.0", "").unwrap();
        let error = reconcile_attempt("1.1.9").unwrap();
        assert!(error.contains("did not complete"));
        assert!(!automatic_attempt_allowed("1.2.0").unwrap());
        // Another launch of the rolled-back client retains both the error and
        // the block. A later release and an explicit retry remain possible.
        assert_eq!(reconcile_attempt("1.1.9").unwrap(), error);
        assert!(automatic_attempt_allowed("1.2.1").unwrap());
        save_attempt("1.2.0", "Authorization cancelled").unwrap();
        assert_eq!(reconcile_attempt("1.1.9").unwrap(), "Authorization cancelled");
        assert!(!automatic_attempt_allowed("1.2.0").unwrap());
        assert_eq!(reconcile_attempt("1.2.0").unwrap(), "");
        assert!(!attempt_path().exists());
        assert!(automatic_attempt_allowed("1.2.1").unwrap());
    }

    #[test]
    fn unreadable_update_state_prevents_another_automatic_handover() {
        let _lock = DATA_DIR.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = TestDataDir::new("update-corrupt-state");
        std::fs::write(attempt_path(), b"interrupted write").unwrap();
        assert!(automatic_attempt_allowed("1.2.0").is_err());
        assert!(reconcile_attempt("1.1.9").is_err());
        // A user-initiated retry can replace the bad record.
        save_attempt("1.2.0", "").unwrap();
        assert!(!automatic_attempt_allowed("1.2.0").unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn authorization_command_survives_applescript_without_interpolation() {
        let command = "printf '%s' \"a path with 'quotes' and \\backslashes\"\n# $HOME; $(touch /do-not-run)\r";
        // Returning a string exercises the AppleScript parser without running
        // the transaction or requesting administrator permission.
        let output = Command::new("/usr/bin/osascript").args(["-e", &format!("return {}", applescript_string(command))]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'), command);
    }

    #[test]
    fn swap_success_and_cli_failure_are_transactional_with_quoted_paths() {
        for fail_cli in [false, true] {
            let dir = std::env::temp_dir().join(format!("asale-swap-'quoted'-{}-{fail_cli}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let cli = dir.join("cli");
            let stage = dir.join("stage");
            std::fs::create_dir_all(&cli).unwrap();
            std::fs::create_dir_all(&stage).unwrap();
            let app = dir.join("Asale.app");
            let prepared = dir.join("prepared.app");
            std::fs::write(&app, "old app").unwrap();
            std::fs::write(&prepared, "new app").unwrap();
            for name in ["asale", "asaled"] {
                std::fs::write(cli.join(name), "old cli").unwrap();
                if !(fail_cli && name == "asaled") { std::fs::write(stage.join(name), "new cli").unwrap(); }
            }
            let q = |p: &std::path::Path| sq(&p.to_string_lossy());
            let script = swap_commands(&q(&app), &q(&prepared), &q(&dir.join("backup")), &q(&cli), &q(&stage));
            let result = Command::new("/bin/sh").args(["-c", &script]).output().unwrap();
            assert_eq!(result.status.success(), !fail_cli);
            assert_eq!(std::fs::read_to_string(&app).unwrap(), if fail_cli { "old app" } else { "new app" });
            for name in ["asale", "asaled"] {
                assert_eq!(std::fs::read_to_string(cli.join(name)).unwrap(), if fail_cli { "old cli" } else { "new cli" });
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn helper_reopens_client_after_failed_swap_and_authorized_install() {
        let dir = std::env::temp_dir().join(format!("asale-helper-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let launched = dir.join("launched");
        let launch = format!("printf '%s' reopened > {}", sq(&launched.to_string_lossy()));
        for transaction in ["exit 1", ":"] {
            let body = unix_helper_body(u32::MAX - 1, transaction, &launch, &dir.join("update.log"));
            let script = dir.join("helper.sh");
            std::fs::write(&script, body).unwrap();
            assert!(Command::new("/bin/sh").arg(&script).status().unwrap().success());
            assert_eq!(std::fs::read_to_string(&launched).unwrap(), "reopened");
            std::fs::remove_file(&launched).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_order_does_not_downgrade_or_accept_invalid_versions() {
        assert!(super::newer("1.2.0", "1.1.9"));
        assert!(!super::newer("1.1.6", "1.1.6"));
        assert!(!super::newer("1.1.6", "1.2.0"));
        assert!(!super::newer("invalid", "1.1.6"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_app_swap_restores_both_cli_binaries_and_application() {
        let dir = std::env::temp_dir().join(format!("asale-rollback-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cli = dir.join("cli");
        let stage = dir.join("stage");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        let app = dir.join("Asale.app");
        std::fs::write(&app, "old app").unwrap();
        for name in ["asale", "asaled"] {
            std::fs::write(cli.join(name), "old cli").unwrap();
            std::fs::write(stage.join(name), "new cli").unwrap();
        }
        let q = |p: &std::path::Path| super::sq(&p.to_string_lossy());
        let script = super::swap_commands(&q(&app), &q(&dir.join("missing")), &q(&dir.join("backup")), &q(&cli), &q(&stage));
        assert!(!std::process::Command::new("/bin/sh").args(["-c", &script]).status().unwrap().success());
        assert_eq!(std::fs::read_to_string(&app).unwrap(), "old app");
        for name in ["asale", "asaled"] { assert_eq!(std::fs::read_to_string(cli.join(name)).unwrap(), "old cli"); }
        std::fs::remove_dir_all(dir).unwrap();
    }

    use super::*;

    /// S6: a cached file that does not hash to what the manifest says is
    /// deleted, not installed; a manifest with no hash is waved through.
    #[test]
    fn a_download_that_does_not_match_the_manifest_hash_is_removed() {
        let dir = std::env::temp_dir().join(format!("asale-updater-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("app.bin");
        std::fs::write(&dest, b"hello").unwrap();
        let mut asset = Asset {
            id: "x".into(),
            name: "app.bin".into(),
            url: String::new(),
            mirror: String::new(),
            size: 5,
            // sha256("hello")
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
        };
        assert!(verify_asset(&asset, &dest).is_ok(), "the right bytes pass");
        assert!(dest.exists());

        asset.sha256 = "00".repeat(32);
        assert!(verify_asset(&asset, &dest).is_err(), "a mismatch is refused");
        assert!(!dest.exists(), "and the file is gone, so nothing can install it");

        std::fs::write(&dest, b"hello").unwrap();
        asset.sha256.clear();
        assert!(verify_asset(&asset, &dest).is_ok(), "no hash in the manifest: warned through");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ASALE_DATA_DIR` is process-global and cargo runs these in threads of one
    /// process, so the tests that repoint it take this lock rather than deleting
    /// each other's scratch directory mid-run.
    static DATA_DIR: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The download half, against a stand-in for asale.ai.
    ///
    /// Three things are worth checking and none of them involve Tauri: the
    /// second address is tried when the first does not answer, a short file is
    /// refused rather than handed to an installer, and the progress the window
    /// draws adds up across both files instead of restarting at each one.
    #[tokio::test]
    async fn a_release_is_fetched_from_whichever_address_answers_and_checked_on_arrival() {
        let _lock = DATA_DIR.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("asale-updater-download-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("ASALE_DATA_DIR", &dir);

        let (addr, hits) = serve().await;
        let base = format!("http://{addr}");
        let cli = wanted_ids()[0];
        let app = wanted_ids()[1];
        let assets = vec![
            Asset {
                id: cli.into(),
                name: "cli.tar.gz".into(),
                // Nothing listening on this port, so the mirror is the only way
                // this file can arrive.
                url: "http://127.0.0.1:1/cli.tar.gz".into(),
                mirror: format!("{base}/cli.tar.gz"),
                size: 8,
                sha256: String::new(),
            },
            Asset {
                id: app.into(),
                name: "app.bin".into(),
                url: format!("{base}/app.bin"),
                mirror: String::new(),
                size: 16,
                sha256: String::new(),
            },
        ];

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, u64, u64)>::new()));
        let sink = seen.clone();
        let out = fetch_release("9.9.9", &assets, move |p| {
            sink.lock().unwrap().push((p.file, p.received, p.total));
        })
        .await
        .expect("downloaded");

        assert_eq!(std::fs::read(out.join("cli.tar.gz")).unwrap().len(), 8, "the mirror was used");
        assert_eq!(std::fs::read(out.join("app.bin")).unwrap().len(), 16);
        assert!(!out.join("cli.tar.gz.part").exists(), "no half-file left behind");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.last().map(|s| (s.1, s.2)), Some((24, 24)), "the bar ends full: {seen:?}");
        assert!(
            seen.windows(2).all(|w| w[0].1 <= w[1].1),
            "progress never goes backwards between files: {seen:?}"
        );

        // Second run: both files are already here at the right size, so nothing
        // is fetched again.
        let before = hits.load(std::sync::atomic::Ordering::SeqCst);
        fetch_release("9.9.9", &assets, |_| {}).await.expect("downloaded");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before, "a complete cache is reused");

        // A file that arrives short is a truncated or intercepted download, and
        // must never be handed to an installer as if it were the release.
        let short = vec![assets[0].clone(), Asset { size: 999, ..assets[1].clone() }];
        let err = fetch_release("9.9.9", &short, |_| {}).await.expect_err("refused");
        assert!(err.contains("app.bin"), "says which file: {err}");
        assert!(!cache_dir("9.9.9").join("app.bin").exists(), "and does not leave it behind");

        std::env::remove_var("ASALE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two fixed-size files, and a counter of how many times they were asked
    /// for. Nothing here needs to be a real release — only the sizes matter.
    async fn serve() -> (std::net::SocketAddr, std::sync::Arc<std::sync::atomic::AtomicU64>) {
        use axum::routing::get;
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (a, b) = (hits.clone(), hits.clone());
        let app = axum::Router::new()
            .route("/cli.tar.gz", get(move || {
                a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { vec![0u8; 8] }
            }))
            .route("/app.bin", get(move || {
                b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { vec![0u8; 16] }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, hits)
    }

}
