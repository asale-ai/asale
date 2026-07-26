//! System tray (spec §12): show/hide the main window, a read-only publish
//! status entry mirroring the real WS session state, and quit. Closing the main
//! window hides it to the tray (see `on_window_event` in lib.rs); quitting is
//! done here. Menu labels stay English on purpose — tray menus follow the OS
//! locale conventions and the in-app UI carries the localized experience.
//!
//! The status entry does not toggle: selling is a per-account decision made on
//! the sell page, and a tray switch that silently took every account off the
//! market would contradict the switches the user actually sees.
//!
//! The shell holds no business state: publish status goes through the daemon's
//! HTTP RPC, exactly like the web UI (loopback requests are trusted).

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

const TOGGLE_WINDOW: &str = "toggle-window";
const PUBLISH_STATUS: &str = "publish-status";
const QUIT: &str = "quit";

/// POST an RPC to the local daemon; returns the JSON result (loopback → no token).
async fn rpc(base: &str, cmd: &str, args: serde_json::Value) -> Option<serde_json::Value> {
    let url = format!("{base}/rpc/{cmd}");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&args)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

pub fn setup(app: &AppHandle, daemon_base: String) -> tauri::Result<()> {
    let toggle_win = MenuItem::with_id(app, TOGGLE_WINDOW, "Show / Hide asale", true, None::<&str>)?;
    // `enabled: false` — a status readout, not a control.
    let publish = MenuItem::with_id(app, PUBLISH_STATUS, "Selling: offline", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "Quit Asale", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle_win, &publish, &PredefinedMenuItem::separator(app)?, &quit])?;

    TrayIconBuilder::with_id("asale-tray")
        .icon(app.default_window_icon().expect("bundled window icon").clone())
        .tooltip("Asale")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            TOGGLE_WINDOW => toggle_main_window(app),
            QUIT => app.exit(0),
            _ => {}
        })
        .build(app)?;

    // Keep the publish entry in sync with the real publisher session state
    // (it can change from the UI, or drop on reconnect/kick).
    let publish_item = publish;
    let base = daemon_base;
    tauri::async_runtime::spawn(async move {
        loop {
            sync_publish_item(&base, &publish_item).await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });

    Ok(())
}

fn toggle_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

/// Reflect the live session state — and how many accounts are behind it — into
/// the tray entry. The account count is the part that makes "offline" readable:
/// with no account switched on, offline is the correct, intended state.
async fn sync_publish_item(base: &str, item: &MenuItem<tauri::Wry>) {
    let text = match rpc(base, "client_status", serde_json::json!({})).await {
        Some(v) => {
            let state = v["publish_state"].as_str().unwrap_or("offline");
            let selling = v["selling"].as_array().map(|a| a.len()).unwrap_or(0);
            let total = v["accounts_total"].as_u64().unwrap_or(0);
            format!("Selling: {state} ({selling}/{total} accounts)")
        }
        None => "Selling: daemon offline".to_string(),
    };
    let _ = item.set_text(text);
}
