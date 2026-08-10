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
//! None of it can run *inside* the app, because the file being replaced is the
//! one this process is executing: on Windows the copy is refused outright, and
//! on macOS the bundle would be swapped underneath a live webview. So the work
//! is handed to a small helper script that is written out, launched detached
//! from this process, and left to it:
//!
//!   1. wait for this pid to disappear (the app quits itself right after),
//!   2. run the installer, elevated,
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

    // A minute of polling, not a fixed sleep: quitting is usually instant, but
    // a window with an unsaved-state prompt or a wedged webview can take a
    // while, and starting the installer early is the one thing that breaks it.
    let wait = format!(
        "n=0\nwhile kill -0 {pid} 2>/dev/null && [ \"$n\" -lt 120 ]; do\n\tsleep 0.5\n\tn=$((n + 1))\ndone\n"
    );

    #[cfg(target_os = "macos")]
    let install = format!(
        // One authorization for the whole install, raised by macOS itself. The
        // `with prompt` text is the only context the dialog gets: it appears
        // after the app has quit, so nothing else on screen explains it.
        "/usr/bin/osascript \
         -e 'do shell script \"/usr/bin/curl -fsSL {INSTALL_SH} | /bin/sh\" \
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
         \tpkexec /usr/bin/env HOME=\"$HOME\" /bin/sh -c 'curl -fsSL {INSTALL_SH} | sh' \
         || echo 'the installer was cancelled or failed'\n\
         else\n\
         \techo 'no pkexec — installing into $HOME/.local/bin instead'\n\
         \tcurl -fsSL {INSTALL_SH} | sh -s -- --prefix \"$HOME/.local/bin\" \
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

/// The PowerShell spelling of the same thing: inside single quotes only the
/// quote itself is special, and it is escaped by doubling.
#[cfg(windows)]
fn pq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// One test, not two: both halves point `ASALE_DATA_DIR` at a scratch
    /// directory, and cargo runs tests in threads of a single process.
    ///
    /// The helper is a shell script assembled from format strings, and it runs
    /// in the one place with no window left to report a syntax error into — so
    /// hand it to `sh -n` here instead, and check that the part the user
    /// actually notices (the app coming back) is in it.
    #[test]
    fn the_helper_reopens_the_app_and_says_so_to_the_next_launch() {
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

        // Read once, then gone: a flag stranded by a crash mid-install would
        // otherwise pull the window to the front on every launch from then on.
        assert!(!take_relaunch_flag(), "no update has run, so nothing to raise");
        mark_relaunch();
        assert!(take_relaunch_flag(), "the launch after an update raises the window");
        assert!(!take_relaunch_flag(), "and only that one launch does");

        std::env::remove_var("ASALE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

