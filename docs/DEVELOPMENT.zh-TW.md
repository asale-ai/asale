# 開發文件

Asale 用戶端的架構、本機開發、打包與發佈。面向要改程式碼或自行建置的人；
只想安裝使用請看 [README](../README.zh-TW.md)。

[English](DEVELOPMENT.md) · [简体中文](DEVELOPMENT.zh-CN.md) · **繁體中文** ·
[日本語](DEVELOPMENT.ja.md)

---

## 架構

```
asale-client/
├─ protocol/    asale-protocol —— wsrelay 線協定，server 與 client 共用的唯一定義
├─ core/        asale-client-core —— 協定用戶端、執行器、本機儲存（可獨立編譯與測試）
│   ├─ ws.rs         簽章握手、supply.declare、心跳、派單分發
│   ├─ executor.rs   注入本機訂閱憑證、串流回傳、解析用量、執行預算
│   ├─ discovery.rs  ToolAdapter：各 CLI 的偵測與設定讀寫
│   ├─ security.rs   裝置 Ed25519 身分
│   └─ store.rs      SQLite；只存 keychain 參照，不存明文憑證
├─ daemon/      asaled —— 全部商業邏輯，本機 HTTP/JSON-RPC :9700
│   ├─ oauth.rs / auth_store.rs   各平台 OAuth 登入與 ~/.asale/auths 隔離存放
│   ├─ proxy.rs                   本機消費代理 :9787（CLI 的接入位址）
│   ├─ publisher.rs               賣出側工作階段、限額、自動停
│   └─ tool_config.rs             改寫/還原各 CLI 設定，含原檔案備份
├─ src-tauri/   Tauri 2 外殼：系統匣、開機自動啟動、自動更新、深層連結 asale://、單一實例
└─ src/         前端 Vite + React 18 + i18next（zh / zh-TW / en / ja，深淺色主題）
```

**邏輯全在 daemon，Tauri 只是殼** —— 所以前端在瀏覽器裡直接開 `http://localhost:9173`
也能跑通全部頁面（daemon 有起著即可），除錯不必每次都開桌面視窗。

---

## 本機開發

需要 Rust（stable）、Node 20+、pnpm。

```bash
pnpm install
pnpm dev:app          # 起 daemon + Tauri 視窗（注入 ASALE_QUOTA_PUBKEY）
cargo test            # workspace 全量測試
cargo test -p asale-client-core
```

> 用 `pnpm dev:app` 而不是 `pnpm tauri dev`：缺了 `ASALE_QUOTA_PUBKEY`，用戶端無法驗證
> 閘道授權，賣出會永遠卡在「正在上線」。

OAuth 用戶端憑證見 [`.env.example`](../.env.example)（Gemini 需要自備，Claude/Codex 有公開預設值）。

---

## 打包

打包參數全部來自 `.env`（`cp .env.example .env` 後填）。這些值在**編譯期**被固定進
二進位檔：桌面端按兩下啟動，沒有 shell 環境，位址與閘道公鑰必須編進去。

```bash
cp .env.example .env      # 填 ASALE_QUOTA_PUBKEY，缺了打出來的用戶端不能賣出

./scripts/package.sh                          # macOS → .dmg（預設 arm64 + x86_64 通用二進位檔）
./scripts/package.sh --bundles deb,appimage   # 在 Linux 上
pwsh scripts/package.ps1                      # 在 Windows 上 → .msi / .exe
./scripts/package.sh --no-sign --debug        # 本機試打：不簽更新包，編譯快很多
```

腳本除了拼 `pnpm tauri build`，還會先擋幾件裝到使用者機器上才會發現的事：位址必須是
https/wss（用戶端執行時同樣拒絕明文遠端位址）、公鑰不能為空、Linux 的
webkit2gtk-4.1 相依套件、macOS 缺 Apple 憑證的提醒。

Tauri 不能跨系統打包：`.dmg` 只能在 macOS 出，`.msi`/`.exe` 只能在 Windows 出，
`.deb`/`.AppImage` 只能在 Linux 出。三平台 = 三台機器，或者推一個 `v*` tag 讓
[`.github/workflows/release.yml`](../.github/workflows/release.yml) 三個 job 一次出全。

產物在 `target/<target>/release/bundle/`，每個安裝包旁邊有一個 `.sig`。

---

## 發佈與自動更新

更新包簽章私鑰是 `asale-updater.key`（gitignored；公鑰已經編在 `tauri.conf.json`
裡）。私鑰遺失或輪換，已安裝的用戶端就再也驗證不了新版本 —— 按生產金鑰管理。

自動更新走 `https://dl.asale.ai/updater/{{target}}/{{current_version}}`，
需回傳標準 Tauri updater JSON，已是最新則 204。

安裝包隨站台一起發佈（站台儲存庫的 `public/download/` 下各存一份）。macOS 包目前
未做 Apple 簽章與公證，使用者第一次開啟需要手動放行。

---

## 相關文件

主儲存庫裡還有一組規格與設計文件，程式碼註解裡的 `spec §x.y` 指向對應的 spec：

- `asale-client-spec.md` —— 用戶端實作規格（how）
- `asale-client-design.md` —— 設計取捨（why）
- `token-trading.md` —— 買家 → 平台 → 賣家的實際鏈路，標註了程式碼位置
- `deploy/README.md` —— 部署、憑證、環境變數與用戶端打包
