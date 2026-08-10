//! asale desktop shell (Tauri 2) — a thin webview over the asale daemon.
//!
//! All native + server logic lives in `asale-daemon` (the same code path that
//! serves Chrome/B-S mode). The shell only:
//!   - ensures a daemon is running (reuses one already listening, else starts
//!     the daemon in-process on the same port),
//!   - shows the webview (the frontend talks to the daemon over HTTP),
//!   - provides desktop niceties: tray, autostart, single-instance, deep
//!     links, window-state.
//!
//! Updating is not in that list: the app and the `asale` command line ship as
//! one release but land separately, and only the published installer replaces
//! both — see `updater`, which runs it rather than doing the work itself.
//!
//! Two windows, both pointed at the same frontend bundle:
//!   `main`   the app
//!   `panel`  the tray overview — the same React app told to render its panel
//!            view (`?view=panel`), so the numbers on it are the numbers on the
//!            dashboard rather than a second implementation that can disagree.

mod tray;
mod updater;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// Settings-store key for the window-close behaviour. Lives in the daemon store
/// (not in a shell-only file) because the Settings page that writes it is the
/// same page in the browser, and a preference that only existed inside the
/// desktop binary would silently reset when it was changed from a browser.
pub const CLOSE_TO_TRAY_KEY: &str = "close_to_tray";

/// The little shared state the shell itself needs. Everything else is the
/// daemon's.
pub struct Shell {
    /// `http://127.0.0.1:<port>` — the shell always reaches the daemon over
    /// loopback, even when the daemon binds wider for remote browser access.
    pub daemon_base: String,
    /// Required on every /rpc call, loopback included.
    pub token: String,
    /// Mirror of `CLOSE_TO_TRAY_KEY`, refreshed by the tray sync loop. An atomic
    /// because the window-close handler is synchronous and must not block on
    /// the daemon to decide whether to close.
    pub close_to_tray: AtomicBool,
    /// Where the tray icon was last clicked, in physical screen coordinates.
    /// The panel sizes itself to its content *after* it is placed, so the
    /// re-anchoring that follows needs the click long after it happened.
    pub panel_anchor: std::sync::Mutex<Option<(f64, f64)>>,
}

/// The panel's width, and the height it opens at before the frontend has
/// measured itself. Only the height ever changes.
const PANEL_W: f64 = 340.0;
const PANEL_H: f64 = 430.0;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // stdout *and* `~/.asale/asale.log`: this build's stdout usually goes
    // nowhere, and the publisher's upstream-rejection lines are only there.
    asale_daemon::logging::init();

    // Where the daemon should be. The shell always talks to it over loopback,
    // even if the daemon binds wider (remote B/S mode).
    let bind = asale_daemon::resolve_bind(None).expect("valid ASALE_BIND");
    let daemon_base = format!("http://127.0.0.1:{}", bind.port());

    // Ensure a daemon: reuse an already-running one (started standalone by the
    // user, e.g. `asale start` on a dev box), otherwise run it inside this
    // process on a dedicated runtime thread.
    {
        let base_hostport = format!("127.0.0.1:{}", bind.port());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("daemon runtime");
            rt.block_on(async move {
                if asale_daemon::probe(&base_hostport).await {
                    tracing::info!("reusing running daemon at {base_hostport}");
                    return;
                }
                match asale_daemon::start(bind).await {
                    Ok(_started) => {
                        // Keep the runtime (and every daemon task) alive.
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        // Most likely a race with an externally started daemon.
                        tracing::warn!("in-process daemon start failed ({e}); assuming an external one");
                    }
                }
            });
        });
    }

    tauri::Builder::default()
        // single-instance must be the first registered plugin: a second launch
        // exits immediately and focuses the already-running window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin({
            // Remember window position/size — but not visibility, so the app
            // never starts silently hidden in the tray after a hidden quit.
            use tauri_plugin_window_state::StateFlags;
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::all() & !StateFlags::VISIBLE)
                // The panel places itself under the tray icon on every open;
                // restoring a remembered position would drop it wherever the
                // menu bar happened to be on the last machine it ran on.
                .with_denylist(&["panel"])
                .build()
        })
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .invoke_handler(tauri::generate_handler![
            show_main_window,
            hide_tray_panel,
            resize_panel,
            open_web_ui,
            web_ui_url,
            quit_app,
            get_close_to_tray,
            set_close_to_tray,
            installer_command,
            latest_release,
            run_installer,
        ])
        .setup(move |app| {
            // The daemon requires its token on every request, loopback
            // included. The shell runs as the same user, so it can read
            // `~/.asale/daemon.token` and hand it to the webview — done as an
            // *initialization* script rather than a post-load `eval` so the
            // token is in place before the first page script can issue an RPC.
            // A browser, which cannot read the file, uses the `#token=` URL the
            // daemon prints at startup instead.
            let token = asale_daemon::load_or_create_token()?;
            let script = format!(
                "try{{localStorage.setItem('asale.daemon.token',{});}}catch(e){{}}",
                serde_json::to_string(&token).unwrap_or_else(|_| "''".into())
            );

            let shell = Arc::new(Shell {
                daemon_base: daemon_base.clone(),
                token: token.clone(),
                // Hide-on-close is the default; the tray loop corrects this
                // within a tick if the user has chosen otherwise.
                close_to_tray: AtomicBool::new(true),
                panel_anchor: std::sync::Mutex::new(None),
            });
            app.manage(shell.clone());

            {
                let cfg = app
                    .config()
                    .app
                    .windows
                    .iter()
                    .find(|w| w.label == "main")
                    .cloned()
                    .ok_or_else(|| anyhow_msg("no `main` window in tauri.conf.json"))?;
                tauri::WebviewWindowBuilder::from_config(app.handle(), &cfg)?
                    .initialization_script(script.clone())
                    .build()?;
            }

            // A launch that follows "restart to update" has to end with a window
            // the user can see. The helper script can only ask the platform to
            // start this build; by then it is an orphan of the process that
            // quit, with no session of its own to activate from, and a window
            // that came up behind the browser they read the release notes in
            // looks exactly like an update that reopened nothing. This process
            // is the one thing that can raise it, so it does.
            //
            // On a timer, not inline: the window is created a few lines up and
            // the webview behind it is not on screen yet, so showing it here
            // would be showing something that is about to be laid out again.
            if updater::take_relaunch_flag() {
                tracing::info!("relaunched after an update — bringing the window to the front");
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    show_main(&handle);
                });
            }

            // The tray overview panel. Built hidden and positioned on each open
            // (see tray::toggle_panel). A failure here is not fatal: the tray
            // menu still works, and a left click falls back to opening the app.
            if let Err(e) = build_panel(app.handle(), &script) {
                tracing::warn!("tray panel window could not be created: {e}");
            }

            // The tray polls the same /rpc surface as the UI, so it needs the
            // token too — without it every status read 401s and the entry is
            // stuck reporting the service as offline.
            tray::setup(app.handle(), shell)?;

            // Deep link (asale://): macOS registers via the bundled Info.plist
            // (tauri.conf.json > plugins.deep-link); Windows/Linux register at
            // runtime. OAuth keeps the loopback callback as the primary route
            // (spec §3.2) — a deep link only surfaces the URL to the webview
            // and brings the window to front.
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                #[cfg(any(windows, target_os = "linux"))]
                {
                    let _ = app.deep_link().register_all();
                }
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    use tauri::Emitter;
                    show_main(&handle);
                    for url in event.urls() {
                        tracing::info!("deep link received: {url}");
                        let _ = handle.emit("deep-link", url.to_string());
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| match window.label() {
            // Closing the main window hides to the tray by default (spec §12);
            // quitting is done from the tray, or from the window if the user has
            // turned that default off in Settings.
            "main" => {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let hide = window
                        .app_handle()
                        .try_state::<Arc<Shell>>()
                        .map(|s| s.close_to_tray.load(Ordering::Relaxed))
                        .unwrap_or(true);
                    if hide {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
            }
            // The panel behaves like a menu: clicking anywhere else dismisses
            // it. Without this it would sit on top of every other window until
            // the tray icon was clicked a second time.
            "panel" => {
                if let tauri::WindowEvent::Focused(false) = event {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn build_panel(app: &AppHandle, script: &str) -> tauri::Result<()> {
    // Same bundle, different view. `WebviewUrl::App` resolves against the dev
    // server in `tauri dev` and against the embedded assets in a build, so this
    // one string works in both.
    //
    // The view is *also* announced through the initialization script. The query
    // string is carried through a `PathBuf`, and path normalisation is not the
    // same on every platform; a global set before the first page script runs is
    // not subject to any of that. main.tsx accepts either.
    let script = format!("window.__ASALE_VIEW__='panel';{script}");
    tauri::WebviewWindowBuilder::new(app, "panel", tauri::WebviewUrl::App("index.html?view=panel".into()))
        .title("Asale")
        // A starting height only: the frontend measures its content and calls
        // `resize_panel`, which it has already done by the first open (the
        // window is built at startup and stays alive, hidden, between opens).
        .inner_size(PANEL_W, PANEL_H)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        .initialization_script(script)
        .build()?;
    Ok(())
}

/// Bring the app window up — from the tray, a second launch, or a deep link.
///
/// `unminimize` matters as much as `show`: a window minimized to the dock is
/// "visible" as far as the platform is concerned, so a plain show/focus on it
/// does nothing at all and the click reads as broken.
pub fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("panel") {
        let _ = win.hide();
    }
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// The URL that opens this daemon's UI in a browser, token included.
///
/// The token is not a convenience: the daemon serves the app only to a browser
/// that has presented it, so a browser sent to the bare address gets the unlock
/// page and nothing else.
fn web_url(app: &AppHandle) -> Result<String, String> {
    let shell = app.try_state::<Arc<Shell>>().ok_or("shell state missing")?;
    // A fragment is not sent in the HTTP request, access log, or Referer.
    Ok(format!("{}/#token={}", shell.daemon_base, shell.token))
}

pub fn open_web(app: &AppHandle) -> Result<String, String> {
    use tauri_plugin_shell::ShellExt;
    let url = web_url(app)?;
    // Deprecated in favour of tauri-plugin-opener, which is a separate plugin
    // and a separate capability entry. Not worth adding a dependency for the one
    // call site the shell has; revisit if more of the app needs to open URLs.
    #[allow(deprecated)]
    app.shell().open(&url, None).map_err(|e| e.to_string())?;
    Ok(url)
}

// ── commands the frontend calls (Settings page + tray panel) ────────────────

#[tauri::command]
fn show_main_window(app: AppHandle) {
    show_main(&app);
}

#[tauri::command]
fn hide_tray_panel(app: AppHandle) {
    if let Some(win) = app.get_webview_window("panel") {
        let _ = win.hide();
    }
}

/// Make the panel window as tall as the panel says it needs to be.
///
/// The panel is a menu, and what a menu contains decides how tall it is: with
/// the daemon down it is a heading and three buttons, with three accounts
/// selling it is twice that. A fixed window makes one of those correct and the
/// rest a rectangle with a hole in it.
///
/// The clamp is not defensive politeness: a measurement taken mid-layout can
/// come back as a handful of pixels or as the whole document, and either one
/// applied to an always-on-top window is a mess the user has to hunt down the
/// tray icon to close.
#[tauri::command]
fn resize_panel(app: AppHandle, height: f64) {
    if !height.is_finite() {
        return;
    }
    let Some(win) = app.get_webview_window("panel") else { return };
    let want = height.clamp(200.0, 640.0).round();

    // Skip a resize that changes nothing: the frontend re-measures on every
    // render, and moving the window on each of those would make the panel
    // twitch while the numbers tick over.
    if let (Ok(size), Ok(scale)) = (win.inner_size(), win.scale_factor()) {
        if (size.to_logical::<f64>(scale).height - want).abs() < 1.0 {
            return;
        }
    }
    if win.set_size(tauri::LogicalSize::new(PANEL_W, want)).is_ok() {
        // Undecorated, so outer == inner and the physical size is just the
        // logical one scaled.
        let scale = win.scale_factor().unwrap_or(1.0);
        tray::reanchor_panel(&app, (PANEL_W * scale) as i32, (want * scale) as i32);
    }
}

#[tauri::command]
fn open_web_ui(app: AppHandle) -> Result<String, String> {
    if let Some(win) = app.get_webview_window("panel") {
        let _ = win.hide();
    }
    open_web(&app)
}

/// The URL without opening it, for "copy link" and for showing what will open.
#[tauri::command]
fn web_ui_url(app: AppHandle) -> Result<String, String> {
    web_url(&app)
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn get_close_to_tray(app: AppHandle) -> bool {
    app.try_state::<Arc<Shell>>()
        .map(|s| s.close_to_tray.load(Ordering::Relaxed))
        .unwrap_or(true)
}

/// Apply the close behaviour immediately. Persisting it is the frontend's job
/// (`set_setting`), which is also what makes the same switch work in a browser;
/// this only keeps the running window from lagging a tick behind the switch the
/// user just flipped.
#[tauri::command]
fn set_close_to_tray(app: AppHandle, value: bool) {
    if let Some(s) = app.try_state::<Arc<Shell>>() {
        s.close_to_tray.store(value, Ordering::Relaxed);
    }
}

/// The installer command "restart to update" runs, so the Settings page can
/// show it before anything is run.
#[tauri::command]
fn installer_command() -> String {
    updater::install_command()
}

/// What the current published release is, per asale.ai.
#[tauri::command]
async fn latest_release() -> Result<updater::Release, String> {
    updater::latest_release().await
}

/// Upgrade in place with the published installer, then reopen the app.
///
/// Quits this process as a side effect — see `updater` for why none of the work
/// can happen inside it.
#[tauri::command]
fn run_installer(app: AppHandle) -> Result<(), String> {
    updater::run(&app)
}

/// A boxed error carrying just a message, for `setup`'s `Box<dyn Error>`.
fn anyhow_msg(msg: &str) -> Box<dyn std::error::Error> {
    Box::<dyn std::error::Error>::from(msg.to_string())
}
