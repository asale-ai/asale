//! Where things live: the data directory, the files this CLI owns inside it,
//! and how the `asaled` binary is found.

use std::path::{Path, PathBuf};

/// Default bind address. **Keep in sync with `asale_daemon::DEFAULT_BIND`** —
/// the CLI cannot depend on the daemon crate (see cli/Cargo.toml), and a
/// disagreement here means `asale status` probing a port nothing listens on.
pub const DEFAULT_BIND: &str = "127.0.0.1:9700";

/// The app data dir — `$ASALE_DATA_DIR` if set, else `~/.asale`.
///
/// **Keep in sync with `asale_daemon::state::data_dir`.** This is not a
/// convenience: the daemon token, the SQLite store and the pid file this CLI
/// writes all live here, so if the two functions ever disagree the CLI starts
/// managing a daemon it cannot then read the token of.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ASALE_DATA_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(home) = home_dir() {
        return home.join(".asale");
    }
    PathBuf::from(".asale")
}

/// `$HOME`, falling back to `%USERPROFILE%` on Windows, where `HOME` is only
/// set by unix-ish toolchains (git-bash, msys) and absent for everyone else.
pub fn home_dir() -> Option<PathBuf> {
    for var in ["HOME", "USERPROFILE"] {
        if let Some(v) = std::env::var_os(var) {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

/// PID of the daemon this CLI started. Absent does not mean "not running": the
/// desktop shell runs a daemon in-process, and a user can run `asaled` by hand.
pub fn pid_file() -> PathBuf {
    data_dir().join("asaled.pid")
}

/// Where a backgrounded daemon's stdout/stderr goes — the only place to look
/// when `asale start` reports the service never came up.
pub fn log_file() -> PathBuf {
    data_dir().join("asaled.log")
}

/// The bind address the last `asale start` used, so `status` / `url` can find
/// the right port after the fact without the flag being repeated.
pub fn bind_file() -> PathBuf {
    data_dir().join("asaled.bind")
}

/// The daemon access token, created by the daemon on first run (mode 0600).
pub fn token_file() -> PathBuf {
    data_dir().join("daemon.token")
}

pub fn read_token() -> Option<String> {
    let t = std::fs::read_to_string(token_file()).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

const ASALED: &str = if cfg!(windows) { "asaled.exe" } else { "asaled" };

/// Locate the `asaled` binary.
///
/// Order matters: the copy shipped next to this CLI is always the matching
/// version (the install script lays them down as a pair), so it outranks
/// whatever an older install left on `PATH`.
pub fn find_asaled() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ASALED_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let side = dir.join(ASALED);
        if side.is_file() {
            return Some(side);
        }
    }
    if let Some(p) = which(ASALED) {
        return Some(p);
    }
    // Everywhere the installers put it, for the case where this CLI is being
    // run from a download directory rather than from its install location.
    let mut candidates: Vec<PathBuf> = ["/usr/local/bin", "/usr/bin", "/opt/asale/bin"]
        .iter()
        .map(|d| Path::new(d).join(ASALED))
        .collect();
    if let Some(home) = home_dir() {
        candidates.push(home.join(".local/bin").join(ASALED));
        candidates.push(home.join(".asale/bin").join(ASALED));
        candidates.push(home.join("AppData/Local/Asale/bin").join(ASALED));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// `PATH` lookup, without shelling out to `which`/`where`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Best guess at whether this machine has a graphical session at all.
///
/// Used only to *inform*: a headless box is told to use web mode rather than
/// being offered a desktop app it cannot draw. Deliberately generous —
/// `DISPLAY` is unset in an SSH session on a perfectly graphical desktop, so a
/// display manager or a graphical systemd default counts too.
pub fn has_desktop() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        return true;
    }
    for var in ["DISPLAY", "WAYLAND_DISPLAY", "XDG_SESSION_TYPE"] {
        if std::env::var_os(var).is_some_and(|v| !v.is_empty() && v != "tty") {
            return true;
        }
    }
    Path::new("/usr/share/xsessions").is_dir() || Path::new("/usr/share/wayland-sessions").is_dir()
}
