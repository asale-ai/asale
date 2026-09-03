//! "Restart to update" — upgrade in place by re-running the published installer.
//!
//! This is not a second copy of the in-app updater beside it. That one replaces
//! the app bundle and nothing else; this runs the exact command the website
//! hands out (`curl -fsSL https://asale.ai/dl/install.sh | sh`, or the
//! PowerShell one on Windows), which also replaces the `asale`/`asaled` command
//! line. Those two halves drift apart on a real machine — the app updates
//! itself while the CLI never does — and the app has no way to fix that from
//! the inside.
//!
//! It happens in two halves, and which half is which is the whole design.
//!
//! **Downloading** is done here, in the app, while the window is still up — see
//! [`download`]. It is the slow part (up to ninety megabytes), it is the part
//! that can fail in ways the user can act on (offline, a captive portal), and it
//! is the only part that has anything to show. Doing it first means the progress
//! bar is real, and the app does not close until the bytes are on disk.
//!
//! **Installing** cannot run inside the app, because the file being replaced is
//! the one this process is executing: on Windows the copy is refused outright,
//! and on macOS the bundle would be swapped underneath a live webview. So that
//! half is handed to a small helper script that is written out, launched
//! detached from this process, and left to it:
//!
//!   1. wait for this pid to disappear (the app quits itself right after),
//!   2. run the installer, elevated — pointed at the already-downloaded files
//!      through `ASALE_DL_CACHE`, so it copies rather than fetching them again,
//!   3. open the app again — whether or not step 2 worked, because an update
//!      the user cancelled must not leave them with no application.
//!
//! Step 3 is two things, not one. Starting the app is the helper's job and it
//! is reliable. Putting it *in front of* the windows the user was left with
//! while the installer ran is not: the helper is by then an orphan with no
//! session of its own to activate from, and an app that came up behind a
//! browser is indistinguishable from an update that reopened nothing. So the
//! helper nudges (`activate`), and the build that comes up raises itself —
//! see `take_relaunch_flag`, which `lib.rs` reads on startup.
//!
//! Elevation belongs to the helper, not to us: `/usr/local/bin` is root-owned
//! and a piped installer has no terminal for `sudo` to prompt on. macOS gets
//! the system authorization dialog (`osascript … with administrator
//! privileges`), Linux the polkit one (`pkexec`), and Windows needs none from
//! us — the command line goes under `%LOCALAPPDATA%` and the desktop installer
//! raises its own UAC prompt.
//!
//! Everything the helper prints goes to `~/.asale/update.log`. By the time the
//! installer is running there is no window left to report into, so that file is
//! the only account of what happened.

use std::path::PathBuf;
use std::process::Command;

use tauri::AppHandle;

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
        ["cli-windows", "windows"]
    } else if std::path::Path::new("/usr/bin/dpkg").exists() {
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
    if wanted.is_empty() {
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

/// Write the helper, start it, and quit.
pub fn run(app: &AppHandle) -> Result<(), String> {
    let script = write_helper()?;
    spawn_detached(&script)?;
    mark_relaunch();
    tracing::info!("update helper started: {}", script.display());

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

#[cfg(unix)]
fn write_helper() -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    let dir = work_dir();
    let path = dir.join("update-helper.sh");
    let log = sq(&dir.join("update.log").to_string_lossy());
    let pid = std::process::id();
    let target = sq(&relaunch_target().to_string_lossy());
    let cache = ready_cache().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    // The macOS install command is a shell string inside an AppleScript string
    // inside a single-quoted shell argument, and a `'` anywhere in the path
    // ends that outermost quoting early — turning the rest of the command into
    // arguments to nothing. A home directory with an apostrophe in it is rare
    // and real; when it happens, forgo the cache rather than the update.
    #[cfg(target_os = "macos")]
    let cache = if cache.contains('\'') {
        tracing::warn!("not handing the installer a cache path containing a quote: {cache}");
        String::new()
    } else {
        cache
    };

    // A minute of polling, not a fixed sleep: quitting is usually instant, but
    // a window with an unsaved-state prompt or a wedged webview can take a
    // while, and starting the installer early is the one thing that breaks it.
    let wait = format!(
        "n=0\nwhile kill -0 {pid} 2>/dev/null && [ \"$n\" -lt 120 ]; do\n\tsleep 0.5\n\tn=$((n + 1))\ndone\n"
    );

    // `ASALE_DL_CACHE` is how the download the window just showed reaches the
    // installer: it copies from there instead of fetching the same bytes again,
    // elevated, with nothing on screen. An installer that has never heard of it
    // ignores it and downloads as before, so an app newer than the live script
    // still updates.
    //
    // Quoted for the shell it lands in, and on macOS for the AppleScript string
    // it is embedded in as well — see `aq`.
    #[cfg(target_os = "macos")]
    let cache_env = if cache.is_empty() { String::new() } else { format!("ASALE_DL_CACHE={} ", aq(&cache)) };
    #[cfg(not(target_os = "macos"))]
    let cache_env = if cache.is_empty() { String::new() } else { format!("ASALE_DL_CACHE={} ", sq(&cache)) };

    #[cfg(target_os = "macos")]
    let install = format!(
        // One authorization for the whole install, raised by macOS itself. The
        // `with prompt` text is the only context the dialog gets: it appears
        // after the app has quit, so nothing else on screen explains it.
        "/usr/bin/osascript \
         -e 'do shell script \"/usr/bin/curl -fsSL {INSTALL_SH} | {cache_env}/bin/sh\" \
         with administrator privileges \
         with prompt \"Asale needs your administrator password to install the update.\"' \
         || echo 'the installer was cancelled or failed'\n"
    );
    #[cfg(target_os = "macos")]
    // By path first, by name second: the installer replaces the bundle where it
    // stands, so the path this build was launched from is still right — and it
    // is the only thing that tells a dev build apart from the release beside it.
    //
    // Then `activate`, because `open` from here only hands a launch request to
    // LaunchServices: the app starts, but nothing promises it starts *in front*
    // of whatever the user turned to while the installer ran. The sleep is for
    // the webview — activating a bundle that has not finished launching does
    // nothing — and AppleScript's `activate` launches the app itself if `open`
    // somehow did not, so this doubles as the last fallback.
    let relaunch = format!(
        "/usr/bin/open {target} || /usr/bin/open -a Asale || echo 'could not reopen Asale'\n\
         sleep 3\n\
         /usr/bin/osascript -e 'tell application \"Asale\" to activate' >/dev/null 2>&1 \
         || echo 'reopened, but could not bring the window to the front'\n"
    );

    #[cfg(not(target_os = "macos"))]
    let install = format!(
        // polkit is the desktop's own way to ask for a password. Where there is
        // none there is also no terminal for sudo to prompt on, so the fallback
        // installs into the user's own prefix rather than failing on
        // /usr/local/bin — a working update in $HOME beats a clean failure.
        //
        // `env HOME=…` because pkexec hands root a root environment, and the
        // installer reads $HOME to find an existing AppImage install.
        "if command -v pkexec >/dev/null 2>&1; then\n\
         \tpkexec /usr/bin/env HOME=\"$HOME\" {cache_env}/bin/sh -c 'curl -fsSL {INSTALL_SH} | sh' \
         || echo 'the installer was cancelled or failed'\n\
         else\n\
         \techo 'no pkexec — installing into $HOME/.local/bin instead'\n\
         \tcurl -fsSL {INSTALL_SH} | {cache_env}sh -s -- --prefix \"$HOME/.local/bin\" \
         || echo 'the installer failed'\n\
         fi\n"
    );
    #[cfg(not(target_os = "macos"))]
    let relaunch = format!("{target} >/dev/null 2>&1 &\n");

    let body = format!(
        "#!/bin/sh\n\
         # Written by the Asale desktop app when \"restart to update\" was pressed.\n\
         # Safe to delete. It is rewritten on every update.\n\
         exec >>{log} 2>&1\n\
         echo \"--- asale update $(date) ---\"\n\
         \n\
         # The installer replaces the very binary this app runs from, so it\n\
         # cannot start until the app is gone.\n\
         {wait}\
         \n\
         {install}\
         \n\
         # Reopened either way: an update the user cancelled must not leave them\n\
         # with no application.\n\
         {relaunch}"
    );

    std::fs::write(&path, body).map_err(|e| format!("could not write the update helper: {e}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("could not make the update helper executable: {e}"))?;
    Ok(path)
}

#[cfg(windows)]
fn write_helper() -> Result<PathBuf, String> {
    let dir = work_dir();
    let path = dir.join("update-helper.ps1");
    let log = pq(&dir.join("update.log").to_string_lossy());
    let pid = std::process::id();
    let target = pq(&relaunch_target().to_string_lossy());
    // See the unix helper: this is how the download the window already showed
    // reaches the installer instead of being fetched a second time.
    let cache_env = match ready_cache() {
        Some(p) => format!("$env:ASALE_DL_CACHE = {}\r\n", pq(&p.to_string_lossy())),
        None => String::new(),
    };

    // Windows PowerShell 5.1 still negotiates SSL3/TLS1.0 by default, which
    // asale.ai refuses — without this the download fails as a protocol error
    // before the installer is ever fetched.
    let body = format!(
        "# Written by the Asale desktop app when \"restart to update\" was pressed.\r\n\
         # Safe to delete. It is rewritten on every update.\r\n\
         $ErrorActionPreference = 'Continue'\r\n\
         try {{ Start-Transcript -Path {log} -Append | Out-Null }} catch {{}}\r\n\
         \r\n\
         # Windows will not overwrite a running .exe at all, so nothing can\r\n\
         # start until the app is gone.\r\n\
         try {{ Wait-Process -Id {pid} -Timeout 60 -ErrorAction SilentlyContinue }} catch {{}}\r\n\
         \r\n\
         try {{ [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 }} catch {{}}\r\n\
         {cache_env}\
         try {{\r\n\
         \t& ([scriptblock]::Create((Invoke-RestMethod -UseBasicParsing '{INSTALL_PS1}')))\r\n\
         }} catch {{\r\n\
         \tWrite-Host \"the installer failed: $($_.Exception.Message)\"\r\n\
         }}\r\n\
         \r\n\
         # Reopened either way: an update that failed must not leave the user\r\n\
         # with no application.\r\n\
         Start-Process -FilePath {target} -ErrorAction SilentlyContinue\r\n\
         try {{ Stop-Transcript | Out-Null }} catch {{}}\r\n"
    );

    std::fs::write(&path, body).map_err(|e| format!("could not write the update helper: {e}"))?;
    Ok(path)
}

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
/// The backslashes look wrong and are not. `\\\"` is a Rust literal for `\"`,
/// which reaches osascript as an escaped quote inside its own string and comes
/// out the other side as a plain `"` around the value — which is what the shell
/// needs to survive a space in the user's home directory.
#[cfg(target_os = "macos")]
fn aq(s: &str) -> String {
    format!("\\\"{}\\\"", s.replace('\\', "\\\\\\\\").replace('"', "\\\\\\\""))
}

/// The PowerShell spelling of the same thing: inside single quotes only the
/// quote itself is special, and it is escaped by doubling.
#[cfg(windows)]
fn pq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(all(test, unix))]
mod tests {
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

    /// One test, not two: both halves point `ASALE_DATA_DIR` at a scratch
    /// directory, and cargo runs tests in threads of a single process.
    ///
    /// The helper is a shell script assembled from format strings, and it runs
    /// in the one place with no window left to report a syntax error into — so
    /// hand it to `sh -n` here instead, and check that the part the user
    /// actually notices (the app coming back) is in it.
    #[test]
    fn the_helper_reopens_the_app_and_says_so_to_the_next_launch() {
        let _lock = DATA_DIR.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("asale-updater-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("ASALE_DATA_DIR", &dir);

        let path = write_helper().expect("helper written");
        let body = std::fs::read_to_string(&path).expect("helper readable");

        let out = Command::new("/bin/sh").arg("-n").arg(&path).output().expect("sh -n ran");
        assert!(out.status.success(), "helper is not valid sh: {}", String::from_utf8_lossy(&out.stderr));

        // The relaunch must come after the installer, unconditionally: an
        // update the user cancelled must still leave them with an app.
        let install = body.find("install.sh").expect("the helper runs the installer");
        let reopen = if cfg!(target_os = "macos") {
            body.find("/usr/bin/open").expect("the helper reopens the app")
        } else {
            body.rfind(&relaunch_target().to_string_lossy().to_string())
                .expect("the helper reopens the app")
        };
        assert!(reopen > install, "the app is reopened before the installer runs");

        // Nothing downloaded yet, so the helper must not mention a cache at
        // all: an installer pointed at an empty directory would find nothing
        // there and fetch everything anyway, having said it would not.
        assert!(!body.contains("ASALE_DL_CACHE"), "no download, no cache: {body}");

        // With a release sitting in the cache it has to be handed over — that
        // is the whole point of downloading before quitting.
        let cached = dir.join("updates").join("9.9.9");
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::write(cached.join("Asale_9.9.9_universal.dmg"), b"not really a dmg").unwrap();
        let path = write_helper().expect("helper written");
        let body = std::fs::read_to_string(&path).expect("helper readable");
        assert!(body.contains("ASALE_DL_CACHE"), "the installer is told where the files are: {body}");
        assert!(body.contains(&cached.to_string_lossy().to_string()), "and it is the right directory");
        let out = Command::new("/bin/sh").arg("-n").arg(&path).output().expect("sh -n ran");
        assert!(out.status.success(), "helper is not valid sh: {}", String::from_utf8_lossy(&out.stderr));

        // Read once, then gone: a flag stranded by a crash mid-install would
        // otherwise pull the window to the front on every launch from then on.
        assert!(!take_relaunch_flag(), "no update has run, so nothing to raise");
        mark_relaunch();
        assert!(take_relaunch_flag(), "the launch after an update raises the window");
        assert!(!take_relaunch_flag(), "and only that one launch does");

        std::env::remove_var("ASALE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        let short = vec![Asset { size: 999, ..assets[1].clone() }];
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

    /// The macOS install command is a shell string inside an AppleScript string
    /// inside a single-quoted shell argument. Three layers of quoting is not
    /// something to reason about and hope: run the real interpreter and see what
    /// comes out the other end.
    ///
    /// The path deliberately has a space in it — `/Users/first last/…` is an
    /// ordinary Mac, and it is exactly what a missing pair of quotes breaks.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_cache_path_survives_applescript_and_the_shell_below_it() {
        let path = "/tmp/asale cache/updates/9.9.9";
        let script = format!(
            "do shell script \"ASALE_DL_CACHE={} /bin/sh -c 'printf %s \\\"$ASALE_DL_CACHE\\\"'\"",
            aq(path)
        );
        let out = Command::new("/usr/bin/osascript").arg("-e").arg(&script).output().expect("osascript ran");
        assert!(
            out.status.success(),
            "osascript rejected {script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), path);
    }
}

