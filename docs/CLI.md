# `asale` — the command line

[English] · [简体中文](CLI.zh-CN.md)

Asale's whole app lives in one service, `asaled`. The desktop window is a webview
over it, and so is any browser — which is why a machine with no desktop is not a
degraded install. It is **web mode**: put the service on a port, open the printed
URL from anywhere.

`asale` is the command that runs that service. It is installed alongside the
desktop app on every platform, and it is the *entire* install on a headless box.

```
curl -fsSL https://asale.ai/dl/install.sh | sh      # macOS / Linux
irm https://asale.ai/dl/install.ps1 | iex           # Windows
```

Re-run the same line to upgrade: it stops the service, replaces the binaries, and
starts it again if it had been running. `asale update` does the same thing without
you having to remember the URL.

---

## Quick start on a server

```sh
curl -fsSL https://asale.ai/dl/install.sh | sh
asale start --web          # listen on every interface, port 9700
asale url                  # the URL to open — access token included
asale autostart enable     # come back automatically after a reboot
```

Open that URL in any browser and you have the full client: sign in, connect
subscriptions, turn the sell switches on, watch the wallet.

> **The token in that URL is the entire authorization.** Anyone holding it can
> read your credentials and spend your balance. Keep the port off the public
> internet — a firewall, a VPN address (`--bind 10.0.0.5:9700`), or a reverse
> proxy with TLS in front of it. On plain HTTP the token travels in the clear.
> To invalidate every URL you have handed out, delete `~/.asale/daemon.token`
> and restart the service.

---

## Commands

| Command | What it does |
|---|---|
| `asale start` | Start the service in the background, wait for it, print its URL |
| `asale stop` | Stop it, giving it ten seconds to leave the market cleanly |
| `asale restart` | Both, keeping the address it was on |
| `asale status` | Running? On what port? Selling what? Autostart on? (`--json` for scripts) |
| `asale logs [-f] [-n N]` | Show or follow the service log |
| `asale open` | Open the app in a browser, starting the service first if needed |
| `asale url` | Print the app URL without opening it |
| `asale desktop` | Launch the desktop app, if it is installed |
| `asale autostart enable\|disable\|status` | Register the service to start with the machine |
| `asale update` | Re-run the online installer |
| `asale uninstall [--purge --yes]` | Remove the service; `--purge` also deletes your data |
| `asale help [command]` | Everything, or one topic (`asale help web` is the headless guide) |

### Options

| Flag | Applies to | Meaning |
|---|---|---|
| `-b`, `--bind <ip:port>` | start / restart / autostart | Address to listen on (default `127.0.0.1:9700`) |
| `-p`, `--port <n>` | start / restart | Change only the port |
| `--web` | start / restart | Listen on every interface — browser access from other machines |
| `-f`, `--foreground` | start | Run in this terminal instead of detaching (containers, debugging) |
| `--json` | status | Machine-readable output |

The address is remembered in `~/.asale/asaled.bind`, so a later `asale start` or
`asale restart` keeps whichever mode you chose.

### Environment

| Variable | Meaning |
|---|---|
| `ASALE_BIND` | Default bind address |
| `ASALE_DATA_DIR` | Where asale keeps its data (default `~/.asale`) |
| `ASALED_BIN` | Path to the `asaled` binary, if it is not next to `asale` or on `PATH` |
| `ASALE_DL_BASE` | Where `asale update` fetches the installer from |

---

## Autostart

`asale autostart enable` registers the **headless service** with the mechanism
each platform's users would look in to remove it:

| Platform | Mechanism |
|---|---|
| macOS | a LaunchAgent at `~/Library/LaunchAgents/ai.asale.asaled.plist` |
| Linux | a systemd unit — `--user` normally, system-wide when run as root; user units also get lingering enabled so they survive logout |
| Windows | the per-user Run key |

This is **not** the same switch as the desktop app's "launch at login", which
starts the window. On a server you want this one; on a laptop you probably want
that one. They can both be on.

---

## Exit codes

`0` success · `1` failed · `2` bad usage · `3` `asale status` ran fine and the
service is **not** running. That last one is what a monitoring check should look
at:

```sh
asale status >/dev/null || echo "asale is down on $(hostname)"
```

---

## Where things are

| Path | What |
|---|---|
| `~/.asale/asale.db` | The local database |
| `~/.asale/daemon.token` | The access token, mode 0600 |
| `~/.asale/asaled.log` | Service output — where `asale logs` reads from |
| `~/.asale/asaled.pid` | PID of a service started by `asale start` |
| `~/.asale/asaled.bind` | The address the last start used |

`asale uninstall` leaves all of it alone unless you add `--purge --yes`. The
device identity in there is what this machine's market reputation is attached to,
so deleting it means starting over as a new device.

---

## When the service is not ours to control

If the port answers but `asale stop` says the service was not started by this CLI,
the desktop app is running it **inside itself** — that is by design, one daemon per
machine. Quit the app from its tray icon and the port frees up.

---

## Building it yourself

Prebuilt binaries are macOS (universal), Windows x86_64 and Linux x86_64. On any
other architecture — an arm64 server, for instance — build the pair from source:

```sh
git clone https://github.com/asale-ai/asale && cd asale
cp .env.example .env          # fill in ASALE_QUOTA_PUBKEY, see the file
./scripts/package.sh --cli-only
```

That produces `asale-cli-<version>-<platform>.tar.gz` containing `asale` and
`asaled`. Neither links webkit or GTK, so this works on a machine with no
graphical libraries installed at all. See
[Development · Packaging](DEVELOPMENT.md#packaging).
