# `asale` 命令行

[English](CLI.md) · [简体中文]

Asale 的全部逻辑都在一个服务里：`asaled`。桌面窗口是它的一层 webview，浏览器也是——
所以一台没有桌面环境的机器并不是「阉割版」，而是**网页模式**：把服务挂到端口上，
在任何地方打开它打印的网址就行。

`asale` 就是管这个服务的命令。装桌面版时会一并装上；在无桌面的机器上，它就是安装的全部。

```
curl -fsSL https://asale.ai/dl/install.sh | sh      # macOS / Linux
irm https://asale.ai/dl/install.ps1 | iex           # Windows
```

同一条命令再跑一遍就是升级：先停服务、换掉二进制、原来在跑的话再起回来。
`asale update` 做的是同一件事，只是不用记网址。

---

## 服务器上的最短路径

```sh
curl -fsSL https://asale.ai/dl/install.sh | sh
asale start --web          # 监听所有网卡，端口 9700
asale url                  # 要打开的网址，已带访问 token
asale autostart enable     # 重启后自动回来
```

用任意浏览器打开那个网址，就是完整的客户端：登录、连接订阅、打开出售开关、看钱包。

> **网址里的 token 就是全部的访问凭证。** 拿到它的人可以读你的凭据、花你的余额。
> 请让这个端口进不了公网——防火墙、只绑 VPN 地址（`--bind 10.0.0.5:9700`），
> 或者前面放一层带 TLS 的反向代理；明文 HTTP 会把 token 直接暴露在网络上。
> 想让已经发出去的网址全部失效：删掉 `~/.asale/daemon.token` 再重启服务。

---

## 命令

| 命令 | 作用 |
|---|---|
| `asale start` | 后台启动服务，等它起来，打印网址 |
| `asale stop` | 停止服务，先给它十秒干净地退出市场 |
| `asale restart` | 停了再起，保持原来的监听地址 |
| `asale status` | 在跑吗？什么端口？在卖什么？开机自启开了吗？（`--json` 给脚本用） |
| `asale logs [-f] [-n N]` | 查看 / 跟随服务日志 |
| `asale open` | 用浏览器打开应用，服务没起会先起 |
| `asale url` | 只打印网址，不打开 |
| `asale desktop` | 启动桌面应用（如果装了） |
| `asale autostart enable\|disable\|status` | 把服务注册为开机启动 |
| `asale update` | 重新跑一遍在线安装脚本 |
| `asale uninstall [--purge --yes]` | 卸载服务；加 `--purge` 连数据一起删 |
| `asale help [命令]` | 全部帮助，或某个专题（`asale help web` 是无桌面模式指南） |

### 选项

| 参数 | 用于 | 含义 |
|---|---|---|
| `-b`, `--bind <ip:port>` | start / restart / autostart | 监听地址（默认 `127.0.0.1:9700`） |
| `-p`, `--port <n>` | start / restart | 只改端口 |
| `--web` | start / restart | 监听所有网卡，供其他机器的浏览器访问 |
| `-f`, `--foreground` | start | 在当前终端前台运行（容器入口、排查启动失败） |
| `--json` | status | 机器可读输出 |

监听地址会记在 `~/.asale/asaled.bind`，所以之后的 `asale start` / `asale restart`
会保持你选的那个模式，不会悄悄退回只听本机。

### 环境变量

| 变量 | 含义 |
|---|---|
| `ASALE_BIND` | 默认监听地址 |
| `ASALE_DATA_DIR` | 数据目录（默认 `~/.asale`） |
| `ASALED_BIN` | `asaled` 的路径（不在 `asale` 旁边也不在 `PATH` 上时） |
| `ASALE_DL_BASE` | `asale update` 去哪里取安装脚本 |

---

## 开机自启

`asale autostart enable` 把**无桌面服务**注册到各平台用户会去找它的地方：

| 平台 | 机制 |
|---|---|
| macOS | `~/Library/LaunchAgents/ai.asale.asaled.plist` 的 LaunchAgent |
| Linux | systemd unit —— 一般是 `--user`，以 root 运行时用系统级；用户级还会开 lingering，退出登录也不会被杀 |
| Windows | 当前用户的 Run 注册表项 |

它和桌面应用设置页里的「开机自启」**不是同一个开关**：那个启动的是窗口。
服务器上要的是这个，笔记本上多半要的是那个，两个可以同时开。

---

## 退出码

`0` 成功 · `1` 失败 · `2` 用法错误 · `3` `asale status` 正常执行，但服务**没在跑**。
最后这个就是监控该看的东西：

```sh
asale status >/dev/null || echo "asale 掉了：$(hostname)"
```

---

## 文件都在哪

| 路径 | 是什么 |
|---|---|
| `~/.asale/asale.db` | 本地数据库 |
| `~/.asale/daemon.token` | 访问 token，权限 0600 |
| `~/.asale/asaled.log` | 服务输出，`asale logs` 读的就是它 |
| `~/.asale/asaled.pid` | `asale start` 起的服务的 PID |
| `~/.asale/asaled.bind` | 上次启动用的监听地址 |

`asale uninstall` 默认一个都不动，除非加 `--purge --yes`。里面的设备身份是这台机器
在市场上的信誉所挂靠的东西，删了就等于从头做一台新设备。

---

## 服务不是这个 CLI 起的

如果端口有响应，但 `asale stop` 说这个服务不是它启动的，那就是桌面应用把服务**跑在自己
进程里**——这是设计如此，一台机器一个守护进程。从托盘图标退出应用，端口就释放了。

---

## 自己构建

预编译产物只有 macOS（通用）、Windows x86_64、Linux x86_64。其他架构（比如 arm64 服务器）
自己编这一对二进制：

```sh
git clone https://github.com/asale-ai/asale && cd asale
cp .env.example .env          # 填 ASALE_QUOTA_PUBKEY，见文件里的说明
./scripts/package.sh --cli-only
```

产出 `asale-cli-<版本>-<平台>.tar.gz`，里面是 `asale` 和 `asaled`。两个都不链接
webkit / GTK，所以在完全没有图形库的机器上也能编、也能跑。
详见[开发文档 · 打包](DEVELOPMENT.zh-CN.md#打包)。
