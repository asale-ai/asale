//! System tray (spec §12): show/hide the main window, a read-only publish
//! status entry mirroring the real WS session state, and quit. Closing the main
//! window hides it to the tray (see `on_window_event` in lib.rs); quitting is
//! done here.
//!
//! Labels follow the language chosen **in the app**, not the OS. The tray is
//! the only part of asale a user sees while the window is hidden, so leaving it
//! English made the product bilingual in the one place it could not be
//! explained; the language lives in the daemon settings store under `language`
//! (the same key the frontend writes), so the sync loop below reads it back and
//! relabels the menu when it changes.
//!
//! The status entry does not toggle: selling is a per-account decision made on
//! the sell page, and a tray switch that silently took every account off the
//! market would contradict the switches the user actually sees.
//!
//! The shell holds no business state: publish status goes through the daemon's
//! HTTP RPC, exactly like the web UI — including its token, which every /rpc
//! call needs, loopback or not.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

const TOGGLE_WINDOW: &str = "toggle-window";
const PUBLISH_STATUS: &str = "publish-status";
const QUIT: &str = "quit";

/// Every user-visible tray string, one struct per locale so a missing
/// translation is a compile error rather than a fallback at runtime — the same
/// shape the server's mail templates use.
struct TrayStrings {
    toggle: &'static str,
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
    toggle: "Show / Hide Asale",
    quit: "Quit Asale",
    selling: "Selling: {state} ({selling}/{total} accounts)",
    daemon_offline: "Selling: service offline",
    state_online: "online",
    state_offline: "offline",
    state_connecting: "connecting",
};

const ZH: TrayStrings = TrayStrings {
    toggle: "显示 / 隐藏 Asale",
    quit: "退出 Asale",
    selling: "出售中：{state}（{selling}/{total} 个账号）",
    daemon_offline: "出售中：服务未运行",
    state_online: "在线",
    state_offline: "离线",
    state_connecting: "连接中",
};

const ZH_TW: TrayStrings = TrayStrings {
    toggle: "顯示 / 隱藏 Asale",
    quit: "結束 Asale",
    selling: "出售中：{state}（{selling}/{total} 個帳號）",
    daemon_offline: "出售中：服務未執行",
    state_online: "上線",
    state_offline: "離線",
    state_connecting: "連線中",
};

const JA: TrayStrings = TrayStrings {
    toggle: "Asale を表示 / 非表示",
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

pub fn setup(app: &AppHandle, daemon_base: String, token: String) -> tauri::Result<()> {
    let s = &EN; // relabelled by the first sync tick, once the daemon answers
    let toggle_win = MenuItem::with_id(app, TOGGLE_WINDOW, s.toggle, true, None::<&str>)?;
    // `enabled: false` — a status readout, not a control.
    let publish = MenuItem::with_id(app, PUBLISH_STATUS, s.daemon_offline, false, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, s.quit, true, None::<&str>)?;
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
    // (it can change from the UI, or drop on reconnect/kick), and the labels in
    // sync with the language the user picked in Settings.
    tauri::async_runtime::spawn(async move {
        loop {
            sync_menu(&daemon_base, &token, &toggle_win, &publish, &quit).await;
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
/// the tray entry, in the app's language. The account count is the part that
/// makes "offline" readable: with no account switched on, offline is the
/// correct, intended state.
async fn sync_menu(
    base: &str,
    token: &str,
    toggle: &MenuItem<tauri::Wry>,
    publish: &MenuItem<tauri::Wry>,
    quit: &MenuItem<tauri::Wry>,
) {
    let locale = rpc(base, token, "get_setting", serde_json::json!({"key": "language"}))
        .await
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let s = strings(&locale);
    let _ = toggle.set_text(s.toggle);
    let _ = quit.set_text(s.quit);

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
    let _ = publish.set_text(text);
}
