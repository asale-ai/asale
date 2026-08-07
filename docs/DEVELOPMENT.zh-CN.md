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

OAuth 客户端凭证见 [`.env.package.example`](../.env.package.example)（Gemini 需要自备，Claude/Codex 有公开默认值）。

### 把这份源码接到系统的 `asale` 命令上

`pnpm dev:app` 管的是桌面窗口。想让**终端里**的 `asale` 也是当前代码（改 CLI、调
`asale start` 的行为、在本机复现无桌面模式），用 `scripts/link.sh`：

```bash
./scripts/link.sh                 # debug 构建，软链到 /usr/local/bin（要 sudo）
./scripts/link.sh --release       # 启动快、体积小，编译慢
./scripts/link.sh --prefix ~/.local/bin   # 不想动 /usr/local/bin，也就不用 sudo
./scripts/link.sh --status        # 现在的 asale / asaled 指向哪里
./scripts/link.sh --unlink        # 撤销：删软链，把备份的正式版放回去
```

装的是**软链**不是拷贝，所以链完之后改代码只要再 `cargo build`，终端里的 `asale`
立刻就是新的，不用重跑脚本。`asale` 和 `asaled` 一起链：只链前者其实也能跑
（`paths::find_asaled()` 会先找自己旁边的 `asaled`，而软链执行时已经解析到
`target/<profile>/`），但那样 `asaled` 这个命令本身还是旧的正式版，两个入口给出
不同版本，排查时非常费解。

链接前会把 `/usr/local/bin` 里正式安装的那两个真身备份到 `~/.asale/link-backup/`，
`--unlink` 时原样放回；`--unlink` 只删指向本仓库的软链，不碰别人装的东西。

编译期注入的值跟打包同源，都读 `./.env.package` —— 缺 `ASALE_QUOTA_PUBKEY` 一样卖不出去。

链上去之后默认还是正式版那套状态（`~/.asale`、`127.0.0.1:9700`），装了桌面版就会
跟它抢端口和数据目录。要井水不犯河水，用上面那张表里的变量：

```bash
ASALE_DATA_DIR=~/.asale-dev ASALE_BIND=127.0.0.1:9701 ASALE_PROXY_PORT=9788 asale start
```

Windows 上没有对应脚本，直接 `cargo run -p asale-cli -- status`。

---

## 打包

打包参数全部来自 `.env.package`（`cp .env.package.example .env.package` 后填）。这些值在**编译期**被固定进
二进制：桌面端双击启动，没有 shell 环境，地址与网关公钥必须编进去。

```bash
cp .env.package.example .env.package      # 填 ASALE_QUOTA_PUBKEY，缺了打出来的客户端不能卖出

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

### 命令行与无桌面归档

每次打包还会出一份 `bundle/cli/asale-cli-<版本>-<平台>.tar.gz`（Windows 上是 `.zip`），
里面是两个二进制：

- **`asaled`** —— 服务本体。客户端的全部逻辑都在它里面，Web UI 也编了进去
  （`rust-embed`，见 `daemon/src/rpc.rs`），所以一台没有桌面的机器
  `asale start --web` 之后用浏览器就能用。
- **`asale`** —— 命令行：start/stop/restart/status、开机自启注册、打印带 token 的网址。
  见 [CLI.zh-CN.md](CLI.zh-CN.md)。

两个都不链接 webkit / GTK —— 这既是它们能装在裸服务器上的原因，也让构建机不需要任何桌面依赖：

> cargo 里命令行的 bin 名是 **`asale-cli`**，打包脚本才把它放进归档改名为 `asale`。
> 桌面壳的二进制本来就叫 `asale`，同一个 workspace 里两个 bin 写同一个 `target/<profile>/`
> 路径会互相覆盖。所以本地是 `cargo run -p asale-cli -- status`，装完是 `asale status`。

```bash
./scripts/package.sh --cli-only     # 只出归档，不打 .dmg/.deb/.AppImage
./scripts/package.sh --no-cli       # 只打安装包，跟以前一样
```

`--cli-only` 仍然会先构建前端：Web UI 是编进 `asaled` 的，跳过 `pnpm build` 出来的服务
只会回一句「没有内嵌 UI」。

`https://asale.ai/dl/install.sh` 下载的就是这份归档，靠站点仓库 `src/lib/downloads.ts`
里的正则匹配 —— 改名要同时改那张表。

---

## 发布与自动更新

更新就是重新跑一遍官方安装脚本。客户端向 `https://asale.ai/dl/manifest.json` 问当前发布版本
（后台每十分钟一次，设置页也可手动触发），跟自己的版本比对，要更新时执行的正是官网给出的那条命令
—— `curl -fsSL https://asale.ai/dl/install.sh | sh`，Windows 是对应的 PowerShell 版本。

没有增量式自动更新，这是有意的：桌面应用和 `asale` / `asaled` 命令行属于同一个发布版本，
却是分别落到机器上的，只有那个安装脚本能把两边一起换掉；自更新器只能修好窗口，会让终端里
悄悄停在上一个版本。代价是要输一次管理员密码（命令行装在 root 所有的目录里），所以流程是
应用关闭 → 提权安装 → 自动重新打开（见 `src-tauri/src/updater.rs`）。

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
