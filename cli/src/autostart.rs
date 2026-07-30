//! Registering the service to start with the system.
//!
//! Each platform gets the mechanism its users would expect to find, because that
//! is also the one they will look in to remove it:
//!
//!   macOS    a LaunchAgent in ~/Library/LaunchAgents
//!   Linux    a systemd unit — `--user` normally, system-wide when run as root
//!   Windows  the per-user Run key
//!
//! This is a *different* switch from the desktop app's "launch at login" (which
//! `tauri-plugin-autostart` owns, and which starts the GUI). This one starts the
//! headless service, which is what a machine with no desktop needs: the box
//! comes back from a reboot already selling, with the web UI on its port.
//!
//! ExecStart points at `asaled` directly rather than at `asale start` — an init
//! system wants a process it can supervise in the foreground, and handing it a
//! launcher that forks and exits makes it supervise nothing.

use crate::{paths, service};
use anyhow::{bail, Result};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What is registered right now, in words the user can act on.
pub enum State {
    Enabled { mechanism: String, path: String },
    Disabled,
    /// No mechanism this build knows how to drive on this system.
    Unsupported { reason: String },
}

impl State {
    pub fn describe(&self) -> String {
        match self {
            State::Enabled { mechanism, .. } => format!("enabled ({mechanism})"),
            State::Disabled => "disabled".to_string(),
            State::Unsupported { reason } => format!("unavailable ({reason})"),
        }
    }
}

#[cfg(target_os = "macos")]
const LABEL: &str = "ai.asale.asaled";
/// Value name under the Windows Run key. Distinct from anything the desktop
/// installer writes, so the two switches cannot delete each other's entry.
#[cfg(windows)]
const WIN_RUN_VALUE: &str = "AsaleService";
#[cfg(windows)]
const WIN_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Only systemd is looked up this way, so on the other two platforms this is
/// dead code rather than a missing feature.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn have(cmd: &str) -> bool {
    paths::which(cmd).is_some() || paths::which(&format!("{cmd}.exe")).is_some()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn running_as_root() -> bool {
    // No libc: the environment is enough here, and being wrong only picks the
    // user unit, which then fails loudly rather than silently doing nothing.
    std::env::var("USER").map(|u| u == "root").unwrap_or(false)
        || std::env::var("LOGNAME").map(|u| u == "root").unwrap_or(false)
        || paths::home_dir().map(|h| h == PathBuf::from("/root")).unwrap_or(false)
}

// ── macOS ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn plist_path() -> Option<PathBuf> {
    Some(paths::home_dir()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn enable_impl(bind: &str) -> Result<State> {
    let exe = paths::find_asaled().context("could not find the `asaled` binary")?;
    let path = plist_path().context("no home directory")?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let log = paths::log_file();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--bind</string>
    <string>{bind}</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>ASALE_DATA_DIR</key><string>{data}</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        data = paths::data_dir().display(),
        log = log.display(),
    );
    std::fs::write(&path, plist).with_context(|| format!("could not write {}", path.display()))?;
    // Unload first: launchd keeps the old definition otherwise, so re-enabling
    // after a bind change would keep the previous address.
    let _ = launchctl(&["unload", "-w"], &path);
    launchctl(&["load", "-w"], &path)?;
    Ok(State::Enabled { mechanism: "launchd LaunchAgent".into(), path: path.display().to_string() })
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str], path: &PathBuf) -> Result<()> {
    let st = Command::new("launchctl")
        .args(args)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !st.success() {
        bail!("launchctl {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn disable_impl() -> Result<()> {
    let path = plist_path().context("no home directory")?;
    if path.exists() {
        let _ = launchctl(&["unload", "-w"], &path);
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn state_impl() -> State {
    match plist_path() {
        Some(p) if p.exists() => {
            State::Enabled { mechanism: "launchd LaunchAgent".into(), path: p.display().to_string() }
        }
        Some(_) => State::Disabled,
        None => State::Unsupported { reason: "no home directory".into() },
    }
}

// ── Linux (systemd) ────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn user_unit() -> Option<PathBuf> {
    Some(paths::home_dir()?.join(".config/systemd/user/asaled.service"))
}

#[cfg(target_os = "linux")]
const SYSTEM_UNIT: &str = "/etc/systemd/system/asaled.service";

/// Prefer the user manager; fall back to the system one when running as root
/// (a headless root shell has no user bus to talk to).
#[cfg(target_os = "linux")]
fn system_scope() -> bool {
    running_as_root() || std::env::var_os("XDG_RUNTIME_DIR").is_none()
}

#[cfg(target_os = "linux")]
fn unit_text(exe: &std::path::Path, bind: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Asale client service (asaled)\n\
         Documentation=https://asale.ai/\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} --bind {bind}\n\
         Environment=ASALE_DATA_DIR={data}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy={target}\n",
        exe = exe.display(),
        data = paths::data_dir().display(),
        target = if system_scope() { "multi-user.target" } else { "default.target" },
    )
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<()> {
    let mut c = Command::new("systemctl");
    if !system_scope() {
        c.arg("--user");
    }
    let st = c.args(args).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    if !st.success() {
        bail!("systemctl {}{} failed", if system_scope() { "" } else { "--user " }, args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn enable_impl(bind: &str) -> Result<State> {
    if !have("systemctl") {
        bail!(
            "no systemd on this system, so there is nothing for asale to register with.\n\
             Start the service from your own init script instead:  asale start --bind {bind}"
        );
    }
    let exe = paths::find_asaled().context("could not find the `asaled` binary")?;
    let path = if system_scope() {
        PathBuf::from(SYSTEM_UNIT)
    } else {
        user_unit().context("no home directory")?
    };
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit_text(&exe, bind))
        .with_context(|| format!("could not write {} (root needed?)", path.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "asaled"])?;
    if !system_scope() {
        // Without lingering, a user unit is killed the moment the SSH session
        // ends — the exact machine this mode exists for.
        let _ = Command::new("loginctl")
            .args(["enable-linger"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mechanism = if system_scope() { "systemd (system)" } else { "systemd (--user)" };
    Ok(State::Enabled { mechanism: mechanism.into(), path: path.display().to_string() })
}

#[cfg(target_os = "linux")]
fn disable_impl() -> Result<()> {
    let path = if system_scope() { PathBuf::from(SYSTEM_UNIT) } else { user_unit().context("no home directory")? };
    if !path.exists() {
        return Ok(());
    }
    if have("systemctl") {
        let _ = systemctl(&["disable", "--now", "asaled"]);
    }
    std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
    if have("systemctl") {
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn state_impl() -> State {
    if !have("systemctl") {
        return State::Unsupported { reason: "no systemd".into() };
    }
    let path = if system_scope() { Some(PathBuf::from(SYSTEM_UNIT)) } else { user_unit() };
    match path {
        Some(p) if p.exists() => State::Enabled {
            mechanism: if system_scope() { "systemd (system)".into() } else { "systemd (--user)".into() },
            path: p.display().to_string(),
        },
        Some(_) => State::Disabled,
        None => State::Unsupported { reason: "no home directory".into() },
    }
}

// ── Windows ────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn enable_impl(bind: &str) -> Result<State> {
    // The Run key runs this CLI, not asaled: `asale start` records the bind,
    // detaches the daemon and exits, which is what a logon task should do.
    let me = std::env::current_exe()?;
    let value = format!("\"{}\" start --bind {bind}", me.display());
    let st = Command::new("reg")
        .args(["add", WIN_RUN_KEY, "/v", WIN_RUN_VALUE, "/t", "REG_SZ", "/d", &value, "/f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !st.success() {
        bail!("could not write the Run key ({WIN_RUN_KEY}\\{WIN_RUN_VALUE})");
    }
    Ok(State::Enabled { mechanism: "Windows Run key".into(), path: format!("{WIN_RUN_KEY}\\{WIN_RUN_VALUE}") })
}

#[cfg(windows)]
fn disable_impl() -> Result<()> {
    let _ = Command::new("reg")
        .args(["delete", WIN_RUN_KEY, "/v", WIN_RUN_VALUE, "/f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(())
}

#[cfg(windows)]
fn state_impl() -> State {
    let out = Command::new("reg")
        .args(["query", WIN_RUN_KEY, "/v", WIN_RUN_VALUE])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => State::Enabled {
            mechanism: "Windows Run key".into(),
            path: format!("{WIN_RUN_KEY}\\{WIN_RUN_VALUE}"),
        },
        _ => State::Disabled,
    }
}

// ── anything else ──────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn enable_impl(_bind: &str) -> Result<State> {
    bail!("asale does not know how to register a boot service on this platform")
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn disable_impl() -> Result<()> {
    Ok(())
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn state_impl() -> State {
    State::Unsupported { reason: "unknown platform".into() }
}

// ── public surface ─────────────────────────────────────────────────────────

pub fn state() -> State {
    state_impl()
}

/// Register the service, using the bind address the install is already on so
/// enabling autostart never silently changes who can reach it.
pub fn enable(explicit_bind: Option<&str>) -> Result<State> {
    let bind = service::resolve_bind(explicit_bind);
    let _ = std::fs::create_dir_all(paths::data_dir());
    let _ = std::fs::write(paths::bind_file(), &bind);
    enable_impl(&bind)
}

pub fn disable() -> Result<()> {
    disable_impl()
}
