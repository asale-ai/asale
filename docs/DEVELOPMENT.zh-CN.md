# 开发文档

Asale 客户端的架构、本地开发、打包与发布。面向要改代码或自行构建的人；
只想安装使用请看 [README](../README.zh-CN.md)。

[English](DEVELOPMENT.md) · **简体中文** · [繁體中文](DEVELOPMENT.zh-TW.md) ·
[日本語](DEVELOPMENT.ja.md)

---

## 架构

```
asale-client/
├─ protocol/    asale-protocol —— wsrelay 线协议，server 与 client 共用的唯一定义
├─ core/        asale-client-core —— 协议客户端、执行器、本地库（可独立编译与测试）
│   ├─ ws.rs         签名握手、supply.declare、心跳、派单分发
│   ├─ executor.rs   注入本机订阅凭证、流式回传、解析用量、执行预算
│   ├─ discovery.rs  ToolAdapter：各 CLI 的探测与配置读写
│   ├─ security.rs   设备 Ed25519 身份
│   └─ store.rs      SQLite；只存 keychain 引用，不存明文凭证
├─ daemon/      asaled —— 全部业务逻辑，本地 HTTP/JSON-RPC :9700
│   ├─ oauth.rs / auth_store.rs   各平台 OAuth 登录与 ~/.asale/auths 隔离存放
│   ├─ proxy.rs                   本地消费代理 :9787（CLI 的接入地址）
│   ├─ publisher.rs               卖出侧会话、限额、自动停
│   └─ tool_config.rs             改写/还原各 CLI 配置，含原文件备份
├─ src-tauri/   Tauri 2 外壳：托盘、开机自启、自动更新、深链 asale://、单实例
└─ src/         前端 Vite + React 18 + i18next（zh / zh-TW / en / ja，深浅色主题）
```

**逻辑全在 daemon，Tauri 只是壳** —— 所以前端在浏览器里直接开 `http://localhost:9173`
也能跑通全部页面（daemon 起着即可），调试不必每次都开桌面窗口。

---

## 本地开发

需要 Rust（stable）、Node 20+、pnpm。

```bash
pnpm install
pnpm dev:app          # 起 daemon + Tauri 窗口（注入 ASALE_QUOTA_PUBKEY）
cargo test            # workspace 全量测试
cargo test -p asale-client-core
```

> 用 `pnpm dev:app` 而不是 `pnpm tauri dev`：缺了 `ASALE_QUOTA_PUBKEY`，客户端无法验证
> 网关授权，卖出会永远卡在「正在上线」。

### 与正式版并存

`dev:app` 已经把整套本地状态挪到开发专用的一份，所以装好的正式版可以同时开着：

| | 正式版 | `pnpm dev:app` |
|---|---|---|
| 数据目录 | `~/.asale` | `~/.asale-dev`（`ASALE_DATA_DIR`） |
| daemon | `127.0.0.1:9700` | `127.0.0.1:9701`（`ASALE_BIND`） |
| 本地代理 | `9787` | `9788`（`ASALE_PROXY_PORT`） |
| bundle identifier | `com.asale.desktop` | `com.asale.desktop.dev`（`src-tauri/tauri.dev.conf.json`） |

identifier 必须分开：单实例插件的锁是 `/tmp/<identifier>_si.sock`，共用的话开发版一启动就会自己退出、把正式版的窗口拉到前台。窗口状态、开机自启项也跟着 identifier 走，一并隔开了。

每个变量都保留 `${VAR:-默认值}`，临时换一套照样能覆盖：

```bash
ASALE_DATA_DIR=~/.asale-staging ASALE_BIND=127.0.0.1:9702 pnpm dev:app
```

单独跑 `pnpm dev`（浏览器调试）时前端默认指向 `127.0.0.1:9700`，要接开发 daemon 得自己带上
`VITE_ASALE_DAEMON=http://127.0.0.1:9701 pnpm dev`；`dev:app` 会把这个变量传给它拉起的 vite，无需额外设置。

仍然共用的只有 CLI 工具自己的配置（`~/.claude`、`~/.codex/config.toml`）—— 订阅和买入本来就是要改这些真实文件，两边会互相覆盖，别同时操作。

OAuth 客户端凭证见 [`.env.example`](../.env.example)（Gemini 需要自备，Claude/Codex 有公开默认值）。

---

## 打包

打包参数全部来自 `.env`（`cp .env.example .env` 后填）。这些值在**编译期**被固定进
二进制：桌面端双击启动，没有 shell 环境，地址与网关公钥必须编进去。

```bash
cp .env.example .env      # 填 ASALE_QUOTA_PUBKEY，缺了打出来的客户端不能卖出

./scripts/package.sh                          # macOS → .dmg（默认 arm64 + x86_64 通用二进制）
./scripts/package.sh --bundles deb,appimage   # 在 Linux 上
pwsh scripts/package.ps1                      # 在 Windows 上 → .msi / .exe
./scripts/package.sh --no-sign --debug        # 本地试打：不签更新包，编译快很多
```

脚本除了拼 `pnpm tauri build`，还会先挡几件装到用户机器上才会发现的事：地址必须是
https/wss（客户端运行时同样拒绝明文远程地址）、公钥不能为空、Linux 的
webkit2gtk-4.1 依赖、macOS 缺 Apple 证书的提醒。

Tauri 不能跨系统打包：`.dmg` 只能在 macOS 出，`.msi`/`.exe` 只能在 Windows 出，
`.deb`/`.AppImage` 只能在 Linux 出。三平台 = 三台机器，或者推一个 `v*` tag 让
[`.github/workflows/release.yml`](../.github/workflows/release.yml) 三个 job 一次出全。

产物在 `target/<target>/release/bundle/`，每个安装包旁边有一个 `.sig`。

---

## 发布与自动更新

更新包签名私钥是 `asale-updater.key`（gitignored；公钥已经编在 `tauri.conf.json`
里）。私钥丢了或轮换，已装的客户端就再也验证不了新版本 —— 按生产密钥管理。

自动更新走 `https://dl.asale.ai/updater/{{target}}/{{current_version}}`，
需返回标准 Tauri updater JSON，已是最新则 204。

安装包随站点一起发布（站点仓库的 `public/download/` 下各存一份）。macOS 包用 Developer ID
证书签名并通过 Apple 公证，Gatekeeper 直接放行 —— 这一步在 `tauri build` 内部完成，依赖
`.github/workflows/release.yml` 里列的那组 `APPLE_*` secret。不配就只能打出 ad-hoc 签名的包，
只能在自己机器上跑。

---

## 相关文档

主仓库里还有一组规格与设计文档，代码注释里的 `spec §x.y` 指向对应的 spec：

- `asale-client-spec.md` —— 客户端实现规格（how）
- `asale-client-design.md` —— 设计取舍（why）
- `token-trading.md` —— 买家 → 平台 → 卖家的实际链路，标注了代码位置
- `deploy/README.md` —— 部署、证书、环境变量与客户端打包
