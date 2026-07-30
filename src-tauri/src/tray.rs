//! System tray (spec §12).
//!
//! Closing the main window hides asale here rather than quitting it (see
//! `on_window_event` in lib.rs), which makes the tray the only part of the app a
//! selling machine shows for days at a time. So it has to answer, without being
//! opened: is this thing working right now? That is the tooltip, refreshed from
//! the same `/rpc/client_status` the UI polls.
//!
//! Two ways in, because the two are asked for at different moments:
//!
//!   left click   the overview panel — a small always-on-top window with the
//!                live figures and three buttons (desktop, browser, quit).
//!                Reading the state is the common case, and a panel can show
//!                far more of it than a menu of text rows.
//!   right click  the plain menu, for when the panel is not what is wanted or
//!                (Linux, some desktop environments) not reliably placeable.
//!
//! Labels follow the language chosen **in the app**, not the OS: the tray is the
//! only part of asale a user sees while the window is hidden, so leaving it
//! English made the product bilingual in the one place it could not be
//! explained. The language lives in the daemon settings store under `language`
//! (the same key the frontend writes), so the sync loop reads it back and
//! relabels the menu when it changes.
//!
//! The status entry does not toggle: selling is a per-account decision made on
//! the sell page, and a tray switch that silently took every account off the
//! market would contradict the switches the user actually sees.
//!
//! The shell holds no business state: everything below goes through the daemon's
//! HTTP RPC, exactly like the web UI — including its token, which every /rpc call
//! needs, loopback or not.

use crate::Shell;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition};

const OPEN_DESKTOP: &str = "open-desktop";
const OPEN_WEB: &str = "open-web";
const PUBLISH_STATUS: &str = "publish-status";
const QUIT: &str = "quit";

/// Every user-visible tray string, one struct per locale so a missing
/// translation is a compile error rather than a fallback at runtime — the same
/// shape the server's mail templates use.
struct TrayStrings {
    open_desktop: &'static str,
    open_web: &'static str,
    quit: &'static str,
    /// `{state}` = publish state, `{selling}`/`{total}` = account counts.
    selling: &'static str,
    daemon_offline: &'static str,
    /// Publish states, as `client_status` reports them.
    state_online: &'static str,
    state_offline: &'static str,
    state_connecting: &'static str,
}

const EN: TrayStrings = TrayStrings {
    open_desktop: "Open Asale",
    open_web: "Open in browser",
    quit: "Quit Asale",
    selling: "Selling: {state} ({selling}/{total} accounts)",
    daemon_offline: "Selling: service offline",
    state_online: "online",
    state_offline: "offline",
    state_connecting: "connecting",
};

const ZH: TrayStrings = TrayStrings {
    open_desktop: "打开 Asale",
    open_web: "在浏览器中打开",
    quit: "退出 Asale",
    selling: "出售中：{state}（{selling}/{total} 个账号）",
    daemon_offline: "出售中：服务未运行",
    state_online: "在线",
    state_offline: "离线",
    state_connecting: "连接中",
};

const ZH_TW: TrayStrings = TrayStrings {
    open_desktop: "開啟 Asale",
    open_web: "在瀏覽器中開啟",
    quit: "結束 Asale",
    selling: "出售中：{state}（{selling}/{total} 個帳號）",
    daemon_offline: "出售中：服務未執行",
    state_online: "上線",
    state_offline: "離線",
    state_connecting: "連線中",
};

const JA: TrayStrings = TrayStrings {
    open_desktop: "Asale を開く",
    open_web: "ブラウザーで開く",
    quit: "Asale を終了",
    selling: "販売中：{state}（{selling}/{total} アカウント）",
    daemon_offline: "販売中：サービス停止中",
    state_online: "オンライン",
    state_offline: "オフライン",
    state_connecting: "接続中",
};

fn strings(locale: &str) -> &'static TrayStrings {
    match locale {
        "zh" => &ZH,
        "zh-TW" => &ZH_TW,
        "ja" => &JA,
        _ => &EN,
    }
}

impl TrayStrings {
    fn state_label(&self, state: &str) -> &'static str {
        match state {
            "online" => self.state_online,
            "connecting" => self.state_connecting,
            _ => self.state_offline,
        }
    }
}

/// POST an RPC to the local daemon; returns the JSON result.
async fn rpc(
    base: &str,
    token: &str,
    cmd: &str,
    args: serde_json::Value,
) -> Option<serde_json::Value> {
    let url = format!("{base}/rpc/{cmd}");
    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-asale-token", token)
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

pub fn setup(app: &AppHandle, shell: Arc<Shell>) -> tauri::Result<()> {
    let s = &EN; // relabelled by the first sync tick, once the daemon answers
    let open_desktop = MenuItem::with_id(app, OPEN_DESKTOP, s.open_desktop, true, None::<&str>)?;
    let open_web = MenuItem::with_id(app, OPEN_WEB, s.open_web, true, None::<&str>)?;
    // `enabled: false` — a status readout, not a control.
    let publish = MenuItem::with_id(app, PUBLISH_STATUS, s.daemon_offline, false, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, s.quit, true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open_desktop,
            &open_web,
            &PredefinedMenuItem::separator(app)?,
            &publish,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let tray = TrayIconBuilder::with_id("asale-tray")
        .icon(app.default_window_icon().expect("bundled window icon").clone())
        .tooltip("Asale")
        .menu(&menu)
        // Left click belongs to the panel; the menu is the right-click surface.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            OPEN_DESKTOP => crate::show_main(app),
            OPEN_WEB => {
                if let Err(e) = crate::open_web(app) {
                    tracing::warn!("could not open the web UI: {e}");
                }
            }
            QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Up, not Down: acting on the press would make a click-and-drag on
            // the menu bar toggle the panel, and on Windows the Down event also
            // arrives for the click that dismisses it.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                position,
                ..
            } = event
            {
                toggle_panel(tray.app_handle(), position);
            }
        })
        .build(app)?;

    // Keep the publish entry and the tooltip in sync with the real publisher
    // session state (it can change from the UI, or drop on reconnect/kick), the
    // labels in sync with the language the user picked in Settings, and the
    // close-to-tray preference in sync with what Settings last wrote.
    tauri::async_runtime::spawn(async move {
        loop {
            sync(&shell, &tray, &open_desktop, &open_web, &publish, &quit).await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });

    Ok(())
}

/// Show the overview panel under the tray icon, or hide it if it is already up.
///
/// Placement is computed from the *cursor*, not from the icon rectangle: the
/// rectangle is reported inconsistently across platforms (and not at all on some
/// Linux desktops), while the click position is always where the user is
/// looking. Which side of the pointer the panel opens on follows the tray's
/// position — top half of the screen means a menu bar above (macOS, GNOME),
/// bottom half means a taskbar below (Windows, KDE).
fn toggle_panel(app: &AppHandle, at: PhysicalPosition<f64>) {
    let Some(win) = app.get_webview_window("panel") else {
        // No panel window (creation failed at startup) — fall back to the thing
        // the user was most likely after.
        crate::show_main(app);
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        return;
    }

    let size = win.outer_size().unwrap_or(tauri::PhysicalSize { width: 340, height: 430 });
    let (mx, my, mw, mh) = match win.current_monitor().ok().flatten() {
        Some(m) => {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width as i32, s.height as i32)
        }
        None => (0, 0, 1920, 1080),
    };
    let (w, h) = (size.width as i32, size.height as i32);
    let margin = 12;
    let near_top = (at.y as i32 - my) < mh / 2;
    let x = (at.x as i32 - w / 2).clamp(mx + margin, (mx + mw - w - margin).max(mx));
    let y = if near_top { at.y as i32 + margin } else { at.y as i32 - h - margin };
    let y = y.clamp(my + margin, (my + mh - h - margin).max(my));

    let _ = win.set_position(PhysicalPosition::new(x, y));
    let _ = win.show();
    let _ = win.set_focus();
}

/// Reflect the live session state — and how many accounts are behind it — into
/// the tray, in the app's language. The account count is the part that makes
/// "offline" readable: with no account switched on, offline is the correct,
/// intended state.
async fn sync(
    shell: &Shell,
    tray: &tauri::tray::TrayIcon,
    open_desktop: &MenuItem<tauri::Wry>,
    open_web: &MenuItem<tauri::Wry>,
    publish: &MenuItem<tauri::Wry>,
    quit: &MenuItem<tauri::Wry>,
) {
    let (base, token) = (&shell.daemon_base, &shell.token);

    let locale = rpc(base, token, "get_setting", serde_json::json!({"key": "language"}))
        .await
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let s = strings(&locale);
    let _ = open_desktop.set_text(s.open_desktop);
    let _ = open_web.set_text(s.open_web);
    let _ = quit.set_text(s.quit);

    // The window-close behaviour has to be readable from a *synchronous* window
    // event handler, so it is mirrored into an atomic here rather than fetched
    // at close time — a close that had to wait on an HTTP round trip would hang
    // the window on a slow daemon.
    if let Some(v) = rpc(base, token, "get_setting", serde_json::json!({"key": crate::CLOSE_TO_TRAY_KEY})).await {
        // Absent means "never set" — the documented default is to hide.
        let hide = v.as_str().map(|s| s != "0").unwrap_or(true);
        shell.close_to_tray.store(hide, std::sync::atomic::Ordering::Relaxed);
    }

    let text = match rpc(base, token, "client_status", serde_json::json!({})).await {
        Some(v) => {
            let state = v["publish_state"].as_str().unwrap_or("offline");
            let selling = v["selling"].as_array().map(|a| a.len()).unwrap_or(0);
            let total = v["accounts_total"].as_u64().unwrap_or(0);
            s.selling
                .replace("{state}", s.state_label(state))
                .replace("{selling}", &selling.to_string())
                .replace("{total}", &total.to_string())
        }
        None => s.daemon_offline.to_string(),
    };
    let _ = publish.set_text(&text);
    // The tooltip is the only status readout available without clicking, which
    // on a machine that is only ever selling is the one that gets read.
    let _ = tray.set_tooltip(Some(format!("Asale — {text}")));
}
