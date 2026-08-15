# `asale` 命令行

[English](CLI.md) · [简体中文]

Asale 的全部逻辑都在一个服务里：`asaled`。桌面窗口是它的一层 webview，浏览器也是——
所以一台没有桌面环境的机器并不是「阉割版」，而是**网页模式**：把服务挂到端口上，
在任何地方打开它打印的网址就行。

`asale` 就是管这个服务的命令。装桌面版时会一并装上；在无桌面的机器上，它就是安装的全部。

```
# macOS / Linux
curl -fsSL https://asale.ai/dl/install.sh | sh
# Windows
irm https://asale.ai/dl/install.ps1 | iex
```

同一条命令再跑一遍就是升级：先停服务、换掉二进制、原来在跑的话再起回来。
`asale update` 做的是同一件事，只是不用记网址。

---

## 服务器上的最短路径

```sh
curl -fsSL https://asale.ai/dl/install.sh | sh
# 启动服务
asale start
# 允许其他机器访问，长期生效
asale expose on
# 要打开的网址，已带访问 token
asale url
# 重启后自动回来
asale autostart enable
```

用任意浏览器打开那个网址，就是完整的客户端：登录、连接订阅、打开出售开关、看钱包。

> **网址里的 token 就是全部的访问凭证。** 把它从网址里删掉，服务只会回一个解锁页，
> 应用和数据一概不给。出示过 token 的浏览器会被记住一天，之后再问一次。
>
> 拿到这个 token 的人可以读你的凭据、花你的余额。请让这个端口进不了公网——防火墙、
> 只绑 VPN 地址（`--bind 10.0.0.5:9700`），或者前面放一层带 TLS 的反向代理；
> 明文 HTTP 会把 token 直接暴露在网络上。
> 想让已经发出去的网址全部失效：删掉 `~/.asale/daemon.token` 再重启服务。

「能被访问」和「访问得到」是两回事：本机防火墙、以及云服务器上的安全组，还得各自放行
这个端口。云主机的公网地址挂在服务商的 NAT 上、不在任何本地网卡上，所以那个网址要用
`asale url --host <公网IP>` 生成。

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
| `asale expose on\|off\|status` | 谁能访问这个服务——立刻生效，且重启后仍然保持 |
| `asale desktop` | 启动桌面应用（如果装了） |
| `asale autostart enable\|disable\|status` | 把服务注册为开机启动 |
| `asale update` | 重新跑一遍在线安装脚本 |
| `asale uninstall [--purge --yes]` | 卸载服务；加 `--purge` 连数据一起删 |
| `asale help [命令]` | 全部帮助，或某个专题（`asale help web` 是无桌面模式指南） |

### 选项

| 参数 | 用于 | 含义 |
|---|---|---|
| `-b`, `--bind <ip:port>` | start / restart / autostart | 监听地址（默认 `127.0.0.1:9700`） |
| `-p`, `--port <n>` | start / restart / expose | 只改端口 |
| `-H`, `--host <ip>` | expose | 只绑一张网卡，例如 VPN 地址 |
| `--host <名字>` | url / open | 用这个主机名拼出网址 |
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

## 谁能访问

`--web` 描述的是某一次启动，`asale expose` 才是它背后的那个设置：写进配置、立刻作用到
正在跑的服务，并同步写进开机自启的定义里，所以一次重启或一次重启机器不会把它悄悄改回去。

```sh
# 所有网卡，端口不变
asale expose on
# 顺便换个端口
asale expose on --port 8080
# 只绑一张网卡——VPN 地址
asale expose on --host 10.0.0.5
# 收回到 127.0.0.1
asale expose off
# 现在听在哪里、谁能访问到
asale expose status
```

服务没在跑时，`on` 会把它拉起来——你要的就是「能访问」；`off` 不会，它只是把已经在跑的
服务收紧。两者都只在地址真的变了时才重启服务，所以重复执行 `expose on` 不会打断一笔正在
进行的交易。

有两样东西它故意不碰：`ASALE_BIND` 的优先级高于这个设置，所以那个变量设了的话，命令会直接
报错而不是假装改好了；防火墙也不归 asale 开。

### 为什么很多时候该让它一直关着

浏览器如果就是你自己的，SSH 隧道比开端口更划算——链路本来就加密，token 不会明文过网络：

```sh
# 开着别关
ssh -N -L 9800:127.0.0.1:9700 user@你的服务器
# 拿到网址，token 已带上
ssh user@你的服务器 'asale url'
```

把那个网址的端口改成 `9800` 再打开——你自己机器上的桌面版可能已经占了 9700。

`expose on` 留给隧道覆盖不了的场景：团队共用的机器、手机、装不了 SSH 客户端的地方。

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

每份定义里都带着自己那份监听地址（`ExecStart=… --bind …`），所以 `asale expose` 改地址时
会一并重写它——否则这个设置只能撑到下次开机，然后就被改回去了。

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
# 填 ASALE_QUOTA_PUBKEY，见文件里的说明
cp .env.package.example .env.package
./scripts/package.sh --cli-only
```

产出 `asale-cli-<版本>-<平台>.tar.gz`，里面是 `asale` 和 `asaled`。两个都不链接
webkit / GTK，所以在完全没有图形库的机器上也能编、也能跑。
详见[开发文档 · 打包](DEVELOPMENT.zh-CN.md#打包)。
