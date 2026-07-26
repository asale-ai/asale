//! asale desktop shell (Tauri 2) — a thin webview over the asale daemon.
//!
//! All native + server logic lives in `asale-daemon` (the same code path that
//! serves Chrome/B-S mode). The shell only:
//!   - ensures a daemon is running (reuses one already listening, else starts
//!     the daemon in-process on the same port),
//!   - shows the webview (the frontend talks to the daemon over HTTP),
//!   - provides desktop niceties: tray, autostart, updater, single-instance,
//!     deep links, window-state.

mod tray;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Where the daemon should be. The shell always talks to it over loopback,
    // even if the daemon binds wider (remote B/S mode).
    let bind = asale_daemon::resolve_bind(None).expect("valid ASALE_BIND");
    let daemon_base = format!("http://127.0.0.1:{}", bind.port());

    // Ensure a daemon: reuse an already-running one (started standalone by the
    // user, e.g. `asaled` on a dev box), otherwise run it inside this process
    // on a dedicated runtime thread.
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
            use tauri::Manager;
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin({
            // Remember window position/size — but not visibility, so the app
            // never starts silently hidden in the tray after a hidden quit.
            use tauri_plugin_window_state::StateFlags;
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::all() & !StateFlags::VISIBLE)
                .build()
        })
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(move |app| {
            // The daemon requires its token on every request, loopback
            // included. The shell runs as the same user, so it can read
            // `~/.asale/daemon.token` and hand it to the webview — done as an
            // *initialization* script rather than a post-load `eval` so the
            // token is in place before the first page script can issue an RPC.
            // A browser, which cannot read the file, uses the `?token=` URL the
            // daemon prints at startup instead.
            {
                let token = asale_daemon::load_or_create_token()?;
                let script = format!(
                    "try{{localStorage.setItem('asale.daemon.token',{});}}catch(e){{}}",
                    serde_json::to_string(&token).unwrap_or_else(|_| "''".into())
                );
                let cfg = app
                    .config()
                    .app
                    .windows
                    .iter()
                    .find(|w| w.label == "main")
                    .cloned()
                    .ok_or_else(|| anyhow_msg("no `main` window in tauri.conf.json"))?;
                tauri::WebviewWindowBuilder::from_config(app.handle(), &cfg)?
                    .initialization_script(script)
                    .build()?;
            }

            tray::setup(app.handle(), daemon_base.clone())?;

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
                    use tauri::{Emitter, Manager};
                    if let Some(win) = handle.get_webview_window("main") {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                    for url in event.urls() {
                        tracing::info!("deep link received: {url}");
                        let _ = handle.emit("deep-link", url.to_string());
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window hides to the tray (spec §12); quitting
            // is done from the tray menu.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// A boxed error carrying just a message, for `setup`'s `Box<dyn Error>`.
fn anyhow_msg(msg: &str) -> Box<dyn std::error::Error> {
    Box::<dyn std::error::Error>::from(msg.to_string())
}
