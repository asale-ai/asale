//! Running the service: start, stop, restart, status, logs, and the web URL.
//!
//! "The service" is `asaled`, the daemon that holds every bit of asale's logic.
//! The desktop app is a webview over it and so is any browser, which is what
//! makes this CLI enough to run asale on a machine with no desktop at all:
//! `asale start --bind 0.0.0.0:9700` and the whole app is on a port.
//!
//! Process supervision is deliberately thin — a pid file, a log file, and the
//! daemon's own `/healthz`. Anything longer-lived than a login session should be
//! registered with the OS instead (`asale autostart enable`, see `autostart.rs`),
//! because an init system does restart-on-crash better than this ever will.

use crate::{autostart, http, paths};
use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Which address the service should be (or already is) on.
///
/// An explicit flag wins, then the environment (so a systemd unit or a Docker
/// image can set it once), then the address the last `start` actually used —
/// that last step is what makes `asale restart` keep web mode instead of
/// silently dropping back to loopback-only.
pub fn resolve_bind(explicit: Option<&str>) -> String {
    if let Some(b) = explicit {
        return b.to_string();
    }
    if let Ok(b) = std::env::var("ASALE_BIND") {
        if !b.trim().is_empty() {
            return b.trim().to_string();
        }
    }
    if let Ok(b) = std::fs::read_to_string(paths::bind_file()) {
        if !b.trim().is_empty() {
            return b.trim().to_string();
        }
    }
    paths::DEFAULT_BIND.to_string()
}

/// Turn `--port 9701` into a bind address, keeping whatever interface the
/// resolved bind already names (so `--port` on a web-mode install stays 0.0.0.0).
pub fn bind_with_port(explicit: Option<&str>, port: u16) -> String {
    let base = resolve_bind(explicit);
    match base.rsplit_once(':') {
        Some((host, _)) => format!("{host}:{port}"),
        None => format!("127.0.0.1:{port}"),
    }
}

pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(paths::pid_file()).ok()?.trim().parse().ok()
}

/// Is `pid` a live process? Signal 0 asks the kernel exactly that; on Windows
/// `tasklist` filtered by pid prints the process only if it exists.
pub fn pid_alive(pid: u32) -> bool {
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn signal(pid: u32, force: bool) -> bool {
    let ok = if cfg!(windows) {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T"]);
        if force {
            c.arg("/F");
        }
        c.stdout(Stdio::null()).stderr(Stdio::null()).status()
    } else {
        Command::new("kill")
            .args([if force { "-KILL" } else { "-TERM" }, &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    ok.map(|s| s.success()).unwrap_or(false)
}

/// The URL that opens the app in a browser.
///
/// The token is not optional and not a convenience: the daemon requires it on
/// every RPC, loopback included, so a browser with no token loads the UI and
/// then fails every call. The desktop shell reads the token file itself, which
/// is why it never shows one in an address bar.
pub fn web_url(bind: &str, host_override: Option<&str>) -> String {
    let addr = http::dial_addr(bind);
    let host = match host_override {
        Some(h) => h.to_string(),
        None => addr.ip().to_string(),
    };
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    match paths::read_token() {
        Some(t) => format!("http://{host}:{}/?token={t}", addr.port()),
        // No token file yet means the daemon has never run. The URL is still
        // worth printing — it is where the app will be — but it will not
        // authorize anything until the service has started once.
        None => format!("http://{host}:{}/", addr.port()),
    }
}

/// True when the resolved bind is reachable from other machines, i.e. the
/// install is in "website mode" and the token is the only thing in the way.
pub fn is_remote(bind: &str) -> bool {
    match bind.parse::<std::net::SocketAddr>() {
        Ok(a) => a.ip().is_unspecified() || !a.ip().is_loopback(),
        Err(_) => false,
    }
}

/// The address another machine on the same network would reach this host at, as
/// far as this host can tell.
///
/// A UDP socket sends nothing when it is "connected" — it only makes the kernel
/// pick a route — so this reads the interface that faces the network without
/// enumerating interfaces or taking a dependency to do it.
///
/// On a cloud VM the answer is the *private* address: the public one lives on
/// the provider's NAT, not on any local interface. So this is printed as a hint
/// next to `asale url --host …`, never as the address to share.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

pub struct StartOutcome {
    pub bind: String,
    pub already_running: bool,
}

pub fn start(explicit_bind: Option<&str>) -> Result<StartOutcome> {
    let bind = resolve_bind(explicit_bind);
    let addr = http::dial_addr(&bind);

    if http::healthy(addr) {
        return Ok(StartOutcome { bind, already_running: true });
    }

    let exe = paths::find_asaled().context(
        "could not find the `asaled` binary.\n\
         Install the asale client (https://asale.ai/dl/), or point ASALED_BIN at it.",
    )?;

    let dir = paths::data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    // Appended, never truncated: the reason a start failed is usually in the
    // *previous* run's last lines.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file())
        .with_context(|| format!("could not open {}", paths::log_file().display()))?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(&exe);
    cmd.arg("--bind")
        .arg(&bind)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    detach(&mut cmd);

    let child = cmd
        .spawn()
        .with_context(|| format!("could not start {}", exe.display()))?;
    let pid = child.id();
    let _ = std::fs::write(paths::pid_file(), pid.to_string());
    let _ = std::fs::write(paths::bind_file(), &bind);

    // Startup is not instant, and the slow part is not local: before it binds,
    // the daemon opens its database, runs pending migrations, and pulls the
    // tradable-model catalog and market prices from the server. Behind a slow or
    // region-blocked link those calls sit on their own timeouts, so a perfectly
    // healthy cold start regularly takes half a minute — which is why the
    // ceiling here is generous, and why the wait says so out loud rather than
    // looking hung. A daemon that died on the way up is caught by its pid, not
    // by the clock.
    let started = std::time::Instant::now();
    let mut announced = false;
    while !http::healthy(addr) {
        if !pid_alive(pid) {
            bail!(
                "the service exited while starting up.\n\
                 Its output is in {} — the last lines say why.",
                paths::log_file().display()
            );
        }
        if !announced && started.elapsed() >= Duration::from_secs(6) {
            announced = true;
            println!("Starting the service (opening the database, pulling the market catalog)…");
        }
        if started.elapsed() >= Duration::from_secs(90) {
            bail!(
                "the service did not answer within 90s. It may still be coming up —\n\
                 check `asale status`, or read {}.",
                paths::log_file().display()
            );
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    Ok(StartOutcome { bind, already_running: false })
}

/// Detach the child so it outlives the shell that started it.
///
/// Unix: its own process group, so the terminal's SIGINT/SIGHUP on close does
/// not reach it. Windows: DETACHED_PROCESS | CREATE_NO_WINDOW, so no console
/// window flashes up and closing the parent console does not kill it.
fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

/// Run the daemon in this terminal instead of backgrounding it — what a systemd
/// unit or a container entrypoint wants, and the quickest way to read a startup
/// failure. Only returns if the daemon exits.
pub fn run_foreground(explicit_bind: Option<&str>) -> Result<i32> {
    let bind = resolve_bind(explicit_bind);
    let exe = paths::find_asaled().context("could not find the `asaled` binary")?;
    let _ = std::fs::write(paths::bind_file(), &bind);
    let status = Command::new(&exe)
        .arg("--bind")
        .arg(&bind)
        .status()
        .with_context(|| format!("could not start {}", exe.display()))?;
    Ok(status.code().unwrap_or(1))
}

pub enum StopOutcome {
    Stopped,
    NotRunning,
    /// Something is serving on the port but this CLI did not start it and there
    /// is no pid to signal — almost always the desktop app's in-process daemon.
    Foreign,
}

pub fn stop() -> Result<StopOutcome> {
    let bind = resolve_bind(None);
    let addr = http::dial_addr(&bind);
    let pid = read_pid().filter(|p| pid_alive(*p));

    let Some(pid) = pid else {
        let _ = std::fs::remove_file(paths::pid_file());
        return Ok(if http::healthy(addr) { StopOutcome::Foreign } else { StopOutcome::NotRunning });
    };

    signal(pid, false);
    // SIGTERM first: the daemon closes its publisher session on the way out, and
    // a device that disappears without saying goodbye is a failed task on the
    // market's side. Only then insist.
    if !http::wait_gone(addr, Duration::from_secs(10)) {
        signal(pid, true);
        http::wait_gone(addr, Duration::from_secs(5));
    }
    let _ = std::fs::remove_file(paths::pid_file());
    Ok(StopOutcome::Stopped)
}

/// What [`rebind`] had to do to put the service on a different address.
pub enum Rebind {
    /// Already there and answering. Restarting would have dropped the market
    /// session for no gain, so it did not.
    Unchanged,
    Restarted,
    /// It was not running, and being reachable was the point of the change.
    Started,
    /// It was not running and starting it was not asked for; the address is
    /// recorded for the next start.
    Saved,
    /// The port is served by a daemon this CLI did not start — the desktop app
    /// runs one in-process. The address is recorded, but only whoever owns that
    /// daemon can put it into effect.
    Foreign,
}

/// Record the address the service should listen on, and make it true now rather
/// than at some later start.
///
/// The write and the stop are ordered, not incidental: [`stop`] waits for the
/// *recorded* address to go quiet, so the file has to keep naming the old one
/// until the old daemon is actually gone. Writing it first would leave `stop`
/// watching a port nothing is listening on — it would return immediately, and
/// the new daemon would race a process still holding the port, which for the
/// usual change (127.0.0.1:9700 → 0.0.0.0:9700, same port) means the new one
/// fails to bind.
pub fn rebind(bind: &str, start_if_stopped: bool) -> Result<Rebind> {
    let current = resolve_bind(None);
    let live = http::healthy(http::dial_addr(&current));

    if live && current == bind {
        save_bind(bind)?;
        return Ok(Rebind::Unchanged);
    }
    if !live {
        save_bind(bind)?;
        if !start_if_stopped {
            return Ok(Rebind::Saved);
        }
        start(Some(bind))?;
        return Ok(Rebind::Started);
    }
    let stopped = stop()?;
    save_bind(bind)?;
    if let StopOutcome::Foreign = stopped {
        return Ok(Rebind::Foreign);
    }
    start(Some(bind))?;
    Ok(Rebind::Restarted)
}

/// Persist the bind address `resolve_bind` will read back.
pub fn save_bind(bind: &str) -> Result<()> {
    let dir = paths::data_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    std::fs::write(paths::bind_file(), bind)
        .with_context(|| format!("could not write {}", paths::bind_file().display()))
}

pub struct Status {
    pub bind: String,
    pub running: bool,
    pub pid: Option<u32>,
    /// True when the port answers but no pid file explains it (desktop app).
    pub foreign: bool,
    pub version: Option<String>,
    /// `/rpc/client_status`, when the token let us read it.
    pub client: Option<serde_json::Value>,
    pub autostart: autostart::State,
}

pub fn status() -> Status {
    let bind = resolve_bind(None);
    let addr = http::dial_addr(&bind);
    let pid = read_pid().filter(|p| pid_alive(*p));
    let health = http::get(addr, "/healthz", Duration::from_millis(1500));
    let running = matches!(&health, Some(r) if r.status == 200);
    let version = health
        .as_ref()
        .and_then(|r| r.json())
        .and_then(|v| v.get("version").and_then(|s| s.as_str().map(str::to_string)));
    let client = if running {
        paths::read_token()
            .and_then(|t| http::rpc(addr, "client_status", &t, Duration::from_secs(5)))
            .filter(|r| r.status == 200)
            .and_then(|r| r.json())
    } else {
        None
    };
    Status {
        bind,
        running,
        pid,
        foreign: running && pid.is_none(),
        version,
        client,
        autostart: autostart::state(),
    }
}

/// Tail the daemon log. `follow` blocks, reprinting as the file grows — the same
/// job as `tail -f`, done here so Windows gets it too.
pub fn logs(lines: usize, follow: bool) -> Result<()> {
    let path = paths::log_file();
    if !path.exists() {
        bail!(
            "no log yet at {} — the service has not been started by `asale start`.",
            path.display()
        );
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(lines).collect();
    for l in tail.iter().rev() {
        println!("{l}");
    }
    if !follow {
        return Ok(());
    }
    let mut seen = text.len() as u64;
    loop {
        std::thread::sleep(Duration::from_millis(700));
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(seen);
        // A shrunk file means it was rotated or truncated: start over from the
        // beginning rather than seeking past the new content.
        if len < seen {
            seen = 0;
        }
        if len == seen {
            continue;
        }
        if let Ok(all) = std::fs::read(&path) {
            let fresh = String::from_utf8_lossy(&all[seen.min(all.len() as u64) as usize..]).into_owned();
            print!("{fresh}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            seen = all.len() as u64;
        }
    }
}

/// Hand a URL to the OS. No `open` crate here — one command per platform is
/// less code than the dependency.
pub fn open_url(url: &str) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        // The empty "" is the window title `start` would otherwise take the URL
        // for; without it a URL with spaces or an `&` opens nothing.
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => bail!("could not open a browser here. Open this URL yourself:\n  {url}"),
    }
}

/// Launch the installed desktop app, for the "I am at the machine after all"
/// case. Absent on a headless box, where web mode is the answer.
pub fn open_desktop() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        for app in ["/Applications/Asale.app", "Applications/Asale.app"] {
            let path = if app.starts_with('/') {
                std::path::PathBuf::from(app)
            } else {
                match paths::home_dir() {
                    Some(h) => h.join(app),
                    None => continue,
                }
            };
            if path.exists() {
                Command::new("open").arg(&path).status()?;
                return Ok(());
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // On Linux the .deb installs the GUI as /usr/bin/asale — the same name
        // as this CLI, which lives in /usr/local/bin and shadows it. Resolving
        // the path explicitly is what keeps `asale desktop` from re-running
        // this CLI in a loop.
        let names: &[&str] = if cfg!(windows) { &["Asale.exe", "asale.exe"] } else { &["asale", "asale-desktop"] };
        let dirs: &[&str] = if cfg!(windows) {
            &[r"C:\Program Files\Asale", r"C:\Program Files (x86)\Asale"]
        } else {
            &["/usr/bin", "/opt/Asale", "/usr/local/lib/asale"]
        };
        let me = std::env::current_exe().ok();
        for d in dirs {
            for n in names {
                let p = std::path::Path::new(d).join(n);
                if p.is_file() && me.as_deref() != Some(p.as_path()) {
                    let mut c = Command::new(&p);
                    detach(&mut c);
                    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
                    return Ok(());
                }
            }
        }
    }
    bail!(
        "the Asale desktop app does not seem to be installed here.\n\
         On a machine with no desktop, use web mode instead: `asale start` then `asale open`."
    )
}
