//! `asale` — the command line for the asale client.
//!
//! Why this exists: every bit of asale's logic lives in `asaled`, and both the
//! desktop app and any browser are thin frontends over it. On a machine with no
//! desktop — a rented GPU box, a home server, a VPS whose only job is to sell
//! idle subscription quota — there is nothing to click, and the app was
//! previously unreachable there even though the daemon it needs runs fine. This
//! binary is that missing half: start the service, put it on a port, register it
//! with the init system, and check on it.
//!
//! It is deliberately a *service* CLI, not a second copy of the app. Selling,
//! buying, prices, the wallet — all of that is one HTTP surface away and already
//! has a UI in four languages; reimplementing it here would mean two places to
//! keep correct. So the rule this file follows is: manage the process, then hand
//! the user the URL.
//!
//! Layout: [`service`] runs the daemon, [`autostart`] registers it with the OS,
//! [`http`] is the loopback client both use, [`paths`] knows where things live.

pub mod autostart;
pub mod http;
pub mod paths;
pub mod service;

use anyhow::{bail, Result};
use std::process::ExitCode;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where `asale update` re-runs the installer from. Overridable so a mirror or
/// a staging site can be pointed at without a rebuild.
fn install_base() -> String {
    std::env::var("ASALE_DL_BASE").unwrap_or_else(|_| "https://asale.ai/dl".into())
}

pub fn main(args: Vec<String>) -> ExitCode {
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            // One line per cause, so an anyhow chain reads as an explanation
            // rather than as one long sentence with colons in it.
            eprintln!("asale: {e}");
            for cause in e.chain().skip(1) {
                eprintln!("       {cause}");
            }
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<u8> {
    let mut it = args.iter().map(String::as_str).peekable();
    let cmd = it.next().unwrap_or("");
    let rest: Vec<&str> = it.collect();

    match cmd {
        "" => {
            print_help(None);
            Ok(0)
        }
        "-h" | "--help" | "help" => {
            print_help(rest.first().copied());
            Ok(0)
        }
        "-V" | "--version" | "version" => {
            println!("asale {VERSION}");
            Ok(0)
        }
        "start" | "up" => cmd_start(&rest),
        "stop" | "down" => cmd_stop(),
        "restart" => cmd_restart(&rest),
        "status" | "st" => cmd_status(&rest),
        "logs" | "log" => cmd_logs(&rest),
        "url" => cmd_url(&rest),
        "open" | "web" => cmd_open(&rest),
        "desktop" | "app" => {
            service::open_desktop()?;
            println!("Opening the Asale desktop app…");
            Ok(0)
        }
        "autostart" | "boot" => cmd_autostart(&rest),
        // The two shapes people reach for before reading the help.
        "enable" | "disable" => {
            let mut with_action = vec![cmd];
            with_action.extend_from_slice(&rest);
            cmd_autostart(&with_action)
        }
        "update" | "upgrade" => cmd_update(),
        "uninstall" => cmd_uninstall(&rest),
        other => {
            eprintln!("asale: unknown command `{other}`\n");
            print_help(None);
            Ok(2)
        }
    }
}

// ── flag plumbing ──────────────────────────────────────────────────────────
//
// A hand-rolled parser rather than clap: nine commands with four flags between
// them is not worth a dependency, and the error messages here can say what to
// do instead of printing a usage block.

fn flag_value<'a>(args: &[&'a str], names: &[&str]) -> Result<Option<&'a str>> {
    let mut i = 0;
    while i < args.len() {
        if names.contains(&args[i]) {
            return match args.get(i + 1) {
                Some(v) if !v.starts_with('-') => Ok(Some(v)),
                _ => bail!("{} needs a value", args[i]),
            };
        }
        i += 1;
    }
    Ok(None)
}

fn has_flag(args: &[&str], names: &[&str]) -> bool {
    args.iter().any(|a| names.contains(a))
}

/// The bind address a command should act on, from `--bind` / `--port`.
fn bind_from(args: &[&str]) -> Result<Option<String>> {
    let explicit = flag_value(args, &["--bind", "-b"])?;
    if let Some(p) = flag_value(args, &["--port", "-p"])? {
        let port: u16 = p.parse().map_err(|_| anyhow::anyhow!("--port must be a number 1-65535, got `{p}`"))?;
        return Ok(Some(service::bind_with_port(explicit, port)));
    }
    // `--web` / `--remote`: shorthand for "serve this to other machines", the
    // whole point of the headless install, so it should not require knowing
    // that 0.0.0.0 is how you spell it.
    if has_flag(args, &["--web", "--remote", "--all-interfaces"]) {
        let base = service::resolve_bind(explicit);
        let port = base.rsplit_once(':').map(|(_, p)| p.to_string()).unwrap_or_else(|| "9700".into());
        return Ok(Some(format!("0.0.0.0:{port}")));
    }
    Ok(explicit.map(str::to_string))
}

// ── commands ───────────────────────────────────────────────────────────────

fn cmd_start(args: &[&str]) -> Result<u8> {
    let bind = bind_from(args)?;
    if has_flag(args, &["--foreground", "-f"]) {
        let code = service::run_foreground(bind.as_deref())?;
        return Ok(code.clamp(0, 255) as u8);
    }
    let out = service::start(bind.as_deref())?;
    if out.already_running {
        println!("The Asale service is already running.");
    } else {
        println!("Asale service started.");
    }
    print_access(&out.bind);
    Ok(0)
}

fn cmd_stop() -> Result<u8> {
    match service::stop()? {
        service::StopOutcome::Stopped => {
            println!("Asale service stopped.");
            Ok(0)
        }
        service::StopOutcome::NotRunning => {
            println!("The Asale service is not running.");
            Ok(0)
        }
        service::StopOutcome::Foreign => {
            // Not an error: the daemon is up and healthy, it just is not ours to
            // signal. Saying "stopped" here would be a lie the user acts on.
            println!(
                "Something is already serving on {}, but it was not started by `asale start`.\n\
                 If that is the Asale desktop app, quit it from the tray icon — it runs the\n\
                 service inside itself.",
                service::resolve_bind(None)
            );
            Ok(1)
        }
    }
}

fn cmd_restart(args: &[&str]) -> Result<u8> {
    let bind = bind_from(args)?;
    if let service::StopOutcome::Foreign = service::stop()? {
        println!(
            "Refusing to restart: the service on {} was not started by this CLI\n\
             (most likely the desktop app runs it in-process). Quit that first.",
            service::resolve_bind(None)
        );
        return Ok(1);
    }
    let out = service::start(bind.as_deref())?;
    println!("Asale service restarted.");
    print_access(&out.bind);
    Ok(0)
}

fn cmd_status(args: &[&str]) -> Result<u8> {
    let s = service::status();
    if has_flag(args, &["--json"]) {
        let doc = serde_json::json!({
            "running": s.running,
            "bind": s.bind,
            "pid": s.pid,
            "external": s.foreign,
            "version": s.version,
            "data_dir": paths::data_dir().display().to_string(),
            "url": service::web_url(&s.bind, None),
            "autostart": s.autostart.describe(),
            "client": s.client,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(if s.running { 0 } else { 3 });
    }

    println!("Asale client service");
    let state = if !s.running {
        "stopped".to_string()
    } else if let Some(pid) = s.pid {
        format!("running (pid {pid})")
    } else {
        "running (started outside this CLI — desktop app?)".to_string()
    };
    row("state", &state);
    row("listening", &s.bind);
    if s.running {
        row("web ui", &service::web_url(&s.bind, None));
    }
    row("data dir", &paths::data_dir().display().to_string());
    if let Some(v) = &s.version {
        row("version", &format!("asaled {v} · asale {VERSION}"));
    } else {
        row("version", &format!("asale {VERSION}"));
    }
    row("autostart", &s.autostart.describe());

    // The daemon's own view of whether being up is currently worth anything.
    // Absent when the token could not be read, which is a real state too: the
    // service is up but this CLI cannot authenticate to it.
    match &s.client {
        Some(c) => {
            let publish = c["publish_state"].as_str().unwrap_or("offline");
            let selling = c["selling"].as_array().map(|a| a.len()).unwrap_or(0);
            let total = c["accounts_total"].as_u64().unwrap_or(0);
            let lanes = c["lanes_selling"].as_u64().unwrap_or(0);
            let blocked = c["lanes_blocked"].as_u64().unwrap_or(0);
            let signed_in = c["signed_in"].as_bool().unwrap_or(false);
            row("account", if signed_in { "signed in" } else { "not signed in — open the web UI to sign in" });
            row(
                "selling",
                &format!("{publish} · {selling}/{total} accounts · {lanes} lanes serving{}",
                    if blocked > 0 { format!(" · {blocked} blocked") } else { String::new() }),
            );
            let buying = c["buying"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            row("buying", if buying.is_empty() { "nothing routed through the proxy" } else { buying.as_str() });
        }
        None if s.running => row("note", "could not read the service token — try again as the user that owns ~/.asale"),
        None => {}
    }

    if !s.running {
        println!("\nStart it with:  asale start");
    }
    Ok(if s.running { 0 } else { 3 })
}

fn cmd_logs(args: &[&str]) -> Result<u8> {
    let n = match flag_value(args, &["-n", "--lines"])? {
        Some(v) => v.parse().map_err(|_| anyhow::anyhow!("-n must be a number, got `{v}`"))?,
        None => 40,
    };
    service::logs(n, has_flag(args, &["-f", "--follow"]))?;
    Ok(0)
}

fn cmd_url(args: &[&str]) -> Result<u8> {
    let bind = service::resolve_bind(bind_from(args)?.as_deref());
    println!("{}", service::web_url(&bind, flag_value(args, &["--host"])?));
    Ok(0)
}

fn cmd_open(args: &[&str]) -> Result<u8> {
    let bind = service::resolve_bind(bind_from(args)?.as_deref());
    // Opening a browser at a dead port shows an error page and teaches the user
    // nothing, so bring the service up first — that is what they meant.
    if !http::healthy(http::dial_addr(&bind)) {
        let out = service::start(Some(&bind))?;
        println!("Asale service started on {}.", out.bind);
    }
    let url = service::web_url(&bind, flag_value(args, &["--host"])?);
    service::open_url(&url)?;
    println!("Opened {url}");
    Ok(0)
}

fn cmd_autostart(args: &[&str]) -> Result<u8> {
    match args.first().copied().unwrap_or("status") {
        "enable" | "on" => {
            let st = autostart::enable(bind_from(&args[1..])?.as_deref())?;
            match &st {
                autostart::State::Enabled { mechanism, path } => {
                    println!("Autostart enabled via {mechanism}.");
                    row("definition", path);
                    row("listening", &service::resolve_bind(None));
                    println!(
                        "\nThis starts the headless service at boot. The desktop app's own\n\
                         \"launch at login\" switch is separate — it starts the window."
                    );
                }
                other => println!("{}", other.describe()),
            }
            Ok(0)
        }
        "disable" | "off" => {
            autostart::disable()?;
            println!("Autostart disabled. The service keeps running until `asale stop`.");
            Ok(0)
        }
        "status" => {
            let st = autostart::state();
            println!("autostart: {}", st.describe());
            if let autostart::State::Enabled { path, .. } = st {
                row("definition", &path);
            }
            Ok(0)
        }
        other => {
            bail!("unknown autostart action `{other}` (enable | disable | status)")
        }
    }
}

fn cmd_update() -> Result<u8> {
    let base = install_base();
    // The installer is the single place that knows how to place files on each
    // platform, so updating re-runs it rather than reimplementing half of it.
    if cfg!(windows) {
        let cmd = format!("irm {base}/install.ps1 | iex");
        println!("Running the online installer…\n  {cmd}");
        let st = std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &cmd])
            .status()?;
        return Ok(if st.success() { 0 } else { 1 });
    }
    let fetch = if paths::which("curl").is_some() {
        format!("curl -fsSL {base}/install.sh")
    } else if paths::which("wget").is_some() {
        format!("wget -qO- {base}/install.sh")
    } else {
        bail!("neither curl nor wget is available; download {base}/install.sh and run it yourself");
    };
    println!("Running the online installer…\n  {fetch} | sh");
    let st = std::process::Command::new("sh").arg("-c").arg(format!("{fetch} | sh")).status()?;
    Ok(if st.success() { 0 } else { 1 })
}

fn cmd_uninstall(args: &[&str]) -> Result<u8> {
    let purge = has_flag(args, &["--purge"]);
    let yes = has_flag(args, &["--yes", "-y"]);
    let data = paths::data_dir();

    if purge && !yes {
        // `~/.asale` is not a cache: it holds the encrypted secret store with
        // the subscription credentials and the device identity this machine's
        // reputation is attached to. Deleting it is not undoable, so it takes a
        // second flag rather than a prompt no piped installer could answer.
        bail!(
            "--purge deletes {} — the local database, the device identity and the\n\
             encrypted credential store. Re-add --yes if that is what you want.",
            data.display()
        );
    }

    let _ = service::stop();
    let _ = autostart::disable();
    println!("Service stopped and autostart removed.");

    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for path in [paths::find_asaled(), std::env::current_exe().ok()].into_iter().flatten() {
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path.display().to_string()),
            Err(e) => kept.push(format!("{} ({e})", path.display())),
        }
    }
    for r in &removed {
        row("removed", r);
    }
    for k in &kept {
        row("left behind", k);
    }
    if !kept.is_empty() {
        println!(
            "\nSome files could not be removed — on Windows a running executable cannot\n\
             delete itself, and a system directory needs an administrator. Delete them by hand."
        );
    }

    if purge {
        std::fs::remove_dir_all(&data).ok();
        row("removed", &data.display().to_string());
    } else {
        println!("\nYour data is still in {} (use --purge --yes to delete it).", data.display());
    }
    println!("The desktop app, if installed, is removed the usual way for your system.");
    Ok(0)
}

// ── output ─────────────────────────────────────────────────────────────────

fn row(k: &str, v: &str) {
    println!("  {k:<11}{v}");
}

/// The two lines a user actually needs after the service comes up: where the app
/// is, and — in web mode — the fact that the token is now the only thing between
/// the network and their wallet.
fn print_access(bind: &str) {
    row("web ui", &service::web_url(bind, None));
    if service::is_remote(bind) {
        let port = bind.rsplit_once(':').map(|(_, p)| p).unwrap_or("9700");
        row("remote", &format!("http://<this-host>:{port}/?token=<token from the URL above>"));
        println!(
            "\nThis service is reachable from other machines. The token in that URL is the\n\
             only thing protecting it — it can spend your balance and read your credentials.\n\
             Keep the port behind a firewall or a reverse proxy with TLS, and do not paste\n\
             the URL anywhere shared."
        );
    }
    if !paths::has_desktop() {
        println!("\nNo desktop session detected — web mode is the right mode here.");
        println!("Survive a reboot with:  asale autostart enable");
    }
}

fn print_help(topic: Option<&str>) {
    if let Some(t) = topic {
        if let Some(text) = help_topic(t) {
            println!("{text}");
            return;
        }
        println!("No help for `{t}`.\n");
    }
    println!(
        "asale {VERSION} — control the Asale client service

USAGE
  asale <command> [options]

SERVICE
  start                Start the service in the background, then print its URL
  stop                 Stop the service
  restart              Stop and start it again
  status               Is it running, what is it selling, is autostart on
  logs [-f] [-n N]     Show (or follow) the service log

ACCESS
  open                 Open the app in a browser (starts the service if needed)
  url                  Print the app URL, including the access token
  desktop              Launch the Asale desktop app, if it is installed

SYSTEM
  autostart enable     Start the service when the machine boots
  autostart disable    Undo that
  autostart status     What is registered right now
  update               Re-run the online installer to get the latest version
  uninstall [--purge]  Remove the service (--purge also deletes your data)

OPTIONS
  -b, --bind <ip:port> Address to listen on (default {default})
  -p, --port <n>       Just change the port
      --web            Listen on every interface, for browser access from
                       another machine. Requires the token; see `asale help web`
  -f, --foreground     start only: run in this terminal instead of detaching
      --json           status only: machine-readable output
  -h, --help [cmd]     This text, or help for one command
  -V, --version        Version

ENVIRONMENT
  ASALE_BIND           Default bind address
  ASALE_DATA_DIR       Where asale keeps its data (default ~/.asale)
  ASALED_BIN           Path to the asaled binary, if it is not on PATH

  On a machine with no desktop, this CLI is the whole client: start the service,
  open the printed URL from any browser. `asale help web` explains that mode.",
        default = paths::DEFAULT_BIND
    );
}

fn help_topic(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "web" | "remote" | "headless" => {
            "asale help web — using Asale from a browser

  Asale's whole app is served by the local service, so a machine with no desktop
  is not a limitation: put the service on a port and use it from any browser.

    asale start --web              listen on every interface (port 9700)
    asale start --web --port 8080  a different port
    asale url                      print the URL, token included
    asale autostart enable         come back automatically after a reboot

  The token in that URL is the entire authorization. Anyone who has it can read
  your credentials and spend your balance, so:

    - keep the port closed to the internet (firewall, or bind to a VPN address
      with `--bind 10.0.0.5:9700`),
    - if it must be public, put a reverse proxy with TLS in front of it — the
      token travels in the URL, and plain HTTP hands it to the network,
    - the token is in ~/.asale/daemon.token; delete that file and restart the
      service to invalidate every URL you have shared."
        }
        "start" => {
            "asale start [--bind ip:port | --port n | --web] [--foreground]

  Starts asaled in the background, waits for it to answer, then prints the URL.
  Already running: says so and prints the URL, so it is safe to run twice.

  The address is remembered in ~/.asale/asaled.bind, so a later `asale start`
  or `asale restart` keeps whatever mode you chose.

  --foreground runs it in this terminal instead — what a container entrypoint
  wants, and the fastest way to read a startup failure."
        }
        "stop" => {
            "asale stop

  Stops the service this CLI started, giving it ten seconds to close its market
  session cleanly before insisting.

  If the port answers but nothing was started here, it is the desktop app: it
  runs the same daemon inside itself. Quit it from the tray icon."
        }
        "autostart" => {
            "asale autostart <enable | disable | status>

  Registers the *headless service* with the system:

    macOS    a LaunchAgent in ~/Library/LaunchAgents
    Linux    a systemd unit — --user normally, system-wide when run as root
             (also enables lingering, so it survives logging out)
    Windows  the per-user Run key

  This is separate from the desktop app's \"launch at login\" switch, which
  starts the window. On a server you want this one; on a laptop you probably
  want that one."
        }
        "uninstall" => {
            "asale uninstall [--purge --yes]

  Stops the service, removes the autostart registration, and deletes the asale
  and asaled binaries.

  Your data — ~/.asale, holding the local database, the device identity and the
  encrypted credential store — is kept unless you add --purge --yes. The device
  identity is what your reputation on the market is attached to, so removing it
  means starting over as a new device.

  The desktop app is uninstalled the usual way for your system (drag Asale.app
  to the Trash, `apt remove asale`, or Add/Remove Programs)."
        }
        _ => return None,
    })
}
