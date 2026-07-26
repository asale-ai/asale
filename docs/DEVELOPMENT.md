# 开发文档

Asale 客户端的架构、本地开发、打包与发布。面向要改代码或自行构建的人；
只想安装使用请看 [README](../README.md)。

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

安装包随站点一起发布（站点仓库的 `public/download/` 下各存一份）。macOS 包目前
未做 Apple 签名与公证，用户首次打开需要手动放行。

---

## 相关文档

主仓库里还有一组规格与设计文档，代码注释里的 `spec §x.y` 指向对应的 spec：

- `asale-client-spec.md` —— 客户端实现规格（how）
- `asale-client-design.md` —— 设计取舍（why）
- `token-trading.md` —— 买家 → 平台 → 卖家的实际链路，标注了代码位置
- `deploy/README.md` —— 部署、证书、环境变量与客户端打包
