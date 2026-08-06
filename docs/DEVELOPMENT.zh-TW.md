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

### 與正式版並存

`dev:app` 已經把整套本機狀態挪到開發專用的一份，所以裝好的正式版可以同時開著：

| | 正式版 | `pnpm dev:app` |
|---|---|---|
| 資料目錄 | `~/.asale` | `~/.asale-dev`（`ASALE_DATA_DIR`） |
| daemon | `127.0.0.1:9700` | `127.0.0.1:9701`（`ASALE_BIND`） |
| 本機代理 | `9787` | `9788`（`ASALE_PROXY_PORT`） |
| bundle identifier | `com.asale.desktop` | `com.asale.desktop.dev`（`src-tauri/tauri.dev.conf.json`） |

identifier 必須分開：單一實例外掛的鎖是 `/tmp/<identifier>_si.sock`，共用的話開發版一啟動就會自己結束、把正式版的視窗拉到前景。視窗狀態、開機自動啟動項也跟著 identifier 走，一併隔開了。

每個變數都保留 `${VAR:-預設值}`，臨時換一套照樣能覆寫：

```bash
ASALE_DATA_DIR=~/.asale-staging ASALE_BIND=127.0.0.1:9702 pnpm dev:app
```

單獨跑 `pnpm dev`（瀏覽器除錯）時前端預設指向 `127.0.0.1:9700`，要接開發 daemon 得自己帶上
`VITE_ASALE_DAEMON=http://127.0.0.1:9701 pnpm dev`；`dev:app` 會把這個變數傳給它啟動的 vite，無需額外設定。

仍然共用的只有 CLI 工具自己的設定（`~/.claude`、`~/.codex/config.toml`）—— 訂閱和買入本來就是要改這些真實檔案，兩邊會互相覆寫，別同時操作。

OAuth 用戶端憑證見 [`.env.package.example`](../.env.package.example)（Gemini 需要自備，Claude/Codex 有公開預設值）。

### 把這份原始碼接到系統的 `asale` 指令上

`pnpm dev:app` 管的是桌面視窗。想讓**終端機裡**的 `asale` 也是目前這份程式碼（改 CLI、
調 `asale start` 的行為、在本機重現無桌面模式），用 `scripts/link.sh`：

```bash
./scripts/link.sh                 # debug 建置，軟連結到 /usr/local/bin（要 sudo）
./scripts/link.sh --release       # 啟動快、體積小，編譯慢
./scripts/link.sh --prefix ~/.local/bin   # 不動 /usr/local/bin，也就不用 sudo
./scripts/link.sh --status        # 現在的 asale / asaled 指向哪裡
./scripts/link.sh --unlink        # 撤銷：刪軟連結，把備份的正式版放回去
```

裝的是**軟連結**不是複製，所以連結完之後改程式碼只要再 `cargo build`，終端機裡的
`asale` 立刻就是新的，不用重跑腳本。`asale` 和 `asaled` 一起連：只連前者其實也能跑
（`paths::find_asaled()` 會先找自己旁邊的 `asaled`，而軟連結執行時已經解析到
`target/<profile>/`），但那樣 `asaled` 這個指令本身還是舊的正式版，兩個入口報出不同
版本，排查時非常費解。

連結前會把 `/usr/local/bin` 裡正式安裝的那兩個本體備份到 `~/.asale/link-backup/`，
`--unlink` 時原樣放回；`--unlink` 只刪指向本倉庫的軟連結，不碰別人裝的東西。

編譯期注入的值跟打包同源，都讀 `./.env.package` —— 缺 `ASALE_QUOTA_PUBKEY` 一樣賣不出去。

連結上去之後預設還是正式版那套狀態（`~/.asale`、`127.0.0.1:9700`），裝了桌面版就會
跟它搶連接埠和資料目錄。要井水不犯河水，用上面那張表裡的變數：

```bash
ASALE_DATA_DIR=~/.asale-dev ASALE_BIND=127.0.0.1:9701 ASALE_PROXY_PORT=9788 asale start
```

Windows 上沒有對應腳本，直接 `cargo run -p asale-cli -- status`。

---

## 打包

打包參數全部來自 `.env.package`（`cp .env.package.example .env.package` 後填）。這些值在**編譯期**被固定進
二進位檔：桌面端按兩下啟動，沒有 shell 環境，位址與閘道公鑰必須編進去。

```bash
cp .env.package.example .env.package      # 填 ASALE_QUOTA_PUBKEY，缺了打出來的用戶端不能賣出

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

### 命令列與無桌面歸檔

每次打包還會出一份 `bundle/cli/asale-cli-<版本>-<平台>.tar.gz`（Windows 上是 `.zip`），
裡面是兩個二進位檔：

- **`asaled`** —— 服務本體。用戶端的全部邏輯都在它裡面，Web UI 也編了進去
  （`rust-embed`，见 `daemon/src/rpc.rs`），所以一台沒有桌面的機器
  `asale start --web` 之後用瀏覽器就能用。
- **`asale`** —— 命令列：start/stop/restart/status、開機自啟註冊、印出帶 token 的網址。
  見 [CLI.md](CLI.md)。

兩個都不連結 webkit / GTK —— 這既是它們能裝在裸伺服器上的原因，也讓建置機不需要任何桌面相依：

> cargo 裡命令列的 bin 名是 **`asale-cli`**，打包腳本才把它放進歸檔改名為 `asale`。
> 桌面殼的二進位檔本來就叫 `asale`，同一個 workspace 裡兩個 bin 寫同一個 `target/<profile>/`
> 路徑會互相覆蓋。所以本機是 `cargo run -p asale-cli -- status`，裝完是 `asale status`。

```bash
./scripts/package.sh --cli-only     # 只出歸檔，不打 .dmg/.deb/.AppImage
./scripts/package.sh --no-cli       # 只打安裝包，跟以前一樣
```

`--cli-only` 仍然會先建置前端：Web UI 是編進 `asaled` 的，跳過 `pnpm build` 出來的服務
只會回一句「沒有內嵌 UI」。

`https://asale.ai/dl/install.sh` 下載的就是這份歸檔，靠站點倉庫 `src/lib/downloads.ts`
裡的正規表示式比對 —— 改名要同時改那張表。

---

## 發佈與自動更新

更新包簽章私鑰是 `asale-updater.key`（gitignored；公鑰已經編在 `tauri.conf.json`
裡）。私鑰遺失或輪換，已安裝的用戶端就再也驗證不了新版本 —— 按生產金鑰管理。

自動更新走 `https://dl.asale.ai/updater/{{target}}/{{current_version}}`，
需回傳標準 Tauri updater JSON，已是最新則 204。

安裝包隨站台一起發佈（站台儲存庫的 `public/download/` 下各存一份）。macOS 包用 Developer ID
憑證簽章並通過 Apple 公證，Gatekeeper 直接放行 —— 這一步在 `tauri build` 內部完成，依賴
`.github/workflows/release.yml` 裡列的那組 `APPLE_*` secret。不設定就只能打出 ad-hoc 簽章的包，
只能在自己機器上跑。

---

## 相關文件

主儲存庫裡還有一組規格與設計文件，程式碼註解裡的 `spec §x.y` 指向對應的 spec：

- `asale-client-spec.md` —— 用戶端實作規格（how）
- `asale-client-design.md` —— 設計取捨（why）
- `token-trading.md` —— 買家 → 平台 → 賣家的實際鏈路，標註了程式碼位置
- `deploy/README.md` —— 部署、憑證、環境變數與用戶端打包
