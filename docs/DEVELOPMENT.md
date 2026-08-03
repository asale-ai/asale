# Development

Architecture, local development, packaging and releasing for the Asale client. Written for
people who want to change the code or build it themselves; if you just want to install and
use it, read the [README](../README.md).

**English** · [简体中文](DEVELOPMENT.zh-CN.md) · [繁體中文](DEVELOPMENT.zh-TW.md) ·
[日本語](DEVELOPMENT.ja.md)

---

## Architecture

```
asale-client/
├─ protocol/    asale-protocol — the wsrelay wire protocol, the single definition shared by server and client
├─ core/        asale-client-core — protocol client, executor, local store (compiles and tests standalone)
│   ├─ ws.rs         signed handshake, supply.declare, heartbeat, job dispatch
│   ├─ executor.rs   injects local subscription credentials, streams back, parses usage, enforces budget
│   ├─ discovery.rs  ToolAdapter: detection and config read/write per CLI
│   ├─ security.rs   device Ed25519 identity
│   └─ store.rs      SQLite; stores keychain references only, never plaintext credentials
├─ daemon/      asaled — all business logic, local HTTP/JSON-RPC on :9700
│   ├─ oauth.rs / auth_store.rs   per-platform OAuth login, isolated storage under ~/.asale/auths
│   ├─ proxy.rs                   local consumer proxy on :9787 (the endpoint CLIs point at)
│   ├─ publisher.rs               sell-side sessions, limits, auto-stop
│   └─ tool_config.rs             rewrites/restores each CLI's config, with a backup of the original
├─ src-tauri/   Tauri 2 shell: tray, launch at login, auto-update, asale:// deep links, single instance
└─ src/         frontend Vite + React 18 + i18next (zh / zh-TW / en / ja, light and dark themes)
```

**All the logic lives in the daemon, Tauri is just a shell** — which is why the frontend
runs every page straight from a browser at `http://localhost:9173` (as long as the daemon
is up), so debugging doesn't require opening the desktop window every time.

---

## Local development

Requires Rust (stable), Node 20+, pnpm.

```bash
pnpm install
pnpm dev:app          # starts daemon + Tauri window (injects ASALE_QUOTA_PUBKEY)
cargo test            # whole workspace
cargo test -p asale-client-core
```

> Use `pnpm dev:app`, not `pnpm tauri dev`: without `ASALE_QUOTA_PUBKEY` the client cannot
> verify the gateway's authorization, and selling stays stuck at "coming online" forever.

### Running alongside the released app

`dev:app` moves every piece of local state to a development-only copy, so an installed
release build can stay open at the same time:

| | Release | `pnpm dev:app` |
|---|---|---|
| Data dir | `~/.asale` | `~/.asale-dev` (`ASALE_DATA_DIR`) |
| Daemon | `127.0.0.1:9700` | `127.0.0.1:9701` (`ASALE_BIND`) |
| Local proxy | `9787` | `9788` (`ASALE_PROXY_PORT`) |
| Bundle identifier | `com.asale.desktop` | `com.asale.desktop.dev` (`src-tauri/tauri.dev.conf.json`) |

The identifier has to differ: the single-instance lock is `/tmp/<identifier>_si.sock`, so
sharing one makes the dev build exit on launch and focus the release window instead. Window
state and the launch-at-login entry are keyed by identifier too, so they separate as well.

Every variable keeps a `${VAR:-default}`, so a one-off set is still just an override:

```bash
ASALE_DATA_DIR=~/.asale-staging ASALE_BIND=127.0.0.1:9702 pnpm dev:app
```

Plain `pnpm dev` (browser debugging) still points the frontend at `127.0.0.1:9700`; to reach
the dev daemon run `VITE_ASALE_DAEMON=http://127.0.0.1:9701 pnpm dev`. The vite server that
`dev:app` starts inherits the variable, so nothing extra is needed there.

What stays shared is the CLI tools' own configuration (`~/.claude`, `~/.codex/config.toml`) —
subscribing and buying exist to rewrite those real files, so the two builds overwrite each
other's. Don't drive both at once.

OAuth client credentials are documented in [`.env.example`](../.env.example) (Gemini
requires your own; Claude/Codex have public defaults).

### Pointing the system `asale` command at this checkout

`pnpm dev:app` covers the desktop window. When the `asale` command **in your terminal**
should be this code too — working on the CLI, on what `asale start` does, or reproducing
headless mode locally — use `scripts/link.sh`:

```bash
./scripts/link.sh                 # debug build, symlinked into /usr/local/bin (needs sudo)
./scripts/link.sh --release       # starts faster and is smaller; slower to compile
./scripts/link.sh --prefix ~/.local/bin   # leave /usr/local/bin alone, and skip sudo
./scripts/link.sh --status        # where asale / asaled currently point
./scripts/link.sh --unlink        # undo: remove the symlinks, restore the backup
```

These are **symlinks, not copies**, so after linking once a plain `cargo build` is enough —
the `asale` in your terminal is the new binary immediately. Both `asale` and `asaled` are
linked: linking only the first would work (`paths::find_asaled()` looks next to itself, and
by the time a symlink runs it has resolved into `target/<profile>/`), but then the `asaled`
command itself would still be the installed release, and two entry points reporting
different versions is a miserable thing to debug.

Anything real already sitting in `/usr/local/bin` is backed up to `~/.asale/link-backup/`
first and restored by `--unlink`, which only ever removes symlinks pointing into this repo.

Compile-time values come from `./.env`, same as packaging — without `ASALE_QUOTA_PUBKEY`
the linked build cannot sell, exactly as a packaged one couldn't.

A linked build still defaults to the release state (`~/.asale`, `127.0.0.1:9700`) and will
fight an installed desktop app over the port and the data directory. Use the variables from
the table above to keep them apart:

```bash
ASALE_DATA_DIR=~/.asale-dev ASALE_BIND=127.0.0.1:9701 ASALE_PROXY_PORT=9788 asale start
```

There is no Windows equivalent; use `cargo run -p asale-cli -- status` there.

---

## Packaging

All packaging parameters come from `.env` (`cp .env.example .env`, then fill it in). These
values are baked into the binary **at compile time**: a desktop app launched by
double-click has no shell environment, so endpoints and the gateway public key must be
compiled in.

```bash
cp .env.example .env      # set ASALE_QUOTA_PUBKEY — without it the built client cannot sell

./scripts/package.sh                          # macOS → .dmg (defaults to an arm64 + x86_64 universal binary)
./scripts/package.sh --bundles deb,appimage   # on Linux
pwsh scripts/package.ps1                      # on Windows → .msi / .exe
./scripts/package.sh --no-sign --debug        # local trial build: no updater signature, much faster
```

Besides assembling the `pnpm tauri build` invocation, the scripts catch a few things up
front that would otherwise only surface on a user's machine: endpoints must be https/wss
(the client also rejects plaintext remote addresses at runtime), the public key must not be
empty, the Linux webkit2gtk-4.1 dependency, and a warning when the code signing credentials
are missing (the Apple certificate on macOS, the Azure ones on Windows).

Tauri cannot cross-compile bundles: `.dmg` only on macOS, `.msi`/`.exe` only on Windows,
`.deb`/`.AppImage` only on Linux. Three platforms = three machines — or push a `v*` tag and
let the three jobs in
[`.github/workflows/release.yml`](../.github/workflows/release.yml) produce all of them at
once.

Output lands in `target/<target>/release/bundle/`, with a `.sig` next to each installer.

### The command line and the headless archive

Every run also produces `bundle/cli/asale-cli-<version>-<platform>.tar.gz` (`.zip` on
Windows), holding two binaries:

- **`asaled`** — the service. All of asale's logic, with the web UI embedded
  (`rust-embed`, see `daemon/src/rpc.rs`), so a machine with no desktop can run
  `asale start --web` and be used from any browser.
- **`asale`** — the command line: start/stop/restart/status, boot registration, and the
  tokenized URL to open. See [CLI.md](CLI.md).

Neither links webkit or GTK, which is what makes them installable on a bare server — and
what lets a build machine produce them without any of the desktop dependencies:

> Cargo builds the command line as **`asale-cli`**; the packaging scripts put it into the
> archive as `asale`. The desktop shell's own binary is already called `asale`, and two bin
> targets in one workspace writing to the same `target/<profile>/` path overwrite each
> other. So `cargo run -p asale-cli -- status` locally, `asale status` once installed.

```bash
./scripts/package.sh --cli-only     # just the archive, no .dmg/.deb/.AppImage
./scripts/package.sh --no-cli       # just the installers, as before
```

`--cli-only` still builds the frontend first: the web UI is compiled *into* `asaled`, so
skipping `pnpm build` would produce a service that answers "no UI embedded".

The archive is what `https://asale.ai/dl/install.sh` downloads, matched by the regexes in
the site repo's `src/lib/downloads.ts` — renaming it means changing that table too.

---

## Releasing and auto-update

The update-bundle signing private key is `asale-updater.key` (gitignored; the public key is
already compiled into `tauri.conf.json`). If that key is lost or rotated, already-installed
clients can never verify a new version again — treat it as a production secret.

Auto-update points at `https://dl.asale.ai/updater/{{target}}/{{current_version}}`, which
must return the standard Tauri updater JSON, or 204 when already up to date.

Installers are published together with the website (a copy of each lives under the site
repo's `public/download/`). macOS bundles are signed with a Developer ID certificate and
notarized by Apple, so Gatekeeper lets them through silently — this happens inside
`tauri build` and needs the `APPLE_*` secrets listed in `.github/workflows/release.yml`.
Build without those and you get an ad-hoc-signed bundle that only runs on your own machine.

### Code signing

Two entirely separate things, easy to conflate:

- the `.sig` next to every artifact is **minisign**, produced from `asale-updater.key`, and
  only the auto-updater cares about it;
- **Authenticode / Developer ID** is what the OS checks when a user double-clicks the
  installer. Without it Windows shows "unknown publisher" and macOS reports the app as
  damaged.

`--no-sign` / `-NoSign` means "sign nothing" — on Windows `package.ps1` also skips the
Authenticode step, so a trial build never needs the Azure credentials.

Windows goes through [Azure Artifact Signing](https://learn.microsoft.com/en-us/azure/trusted-signing/)
(formerly Trusted Signing): the private key stays in Microsoft's HSM, so the build machine
only holds an App Registration client secret and there is no `.pfx` to leak. The six
variables are listed in `.env.example`; `package.ps1` turns them into a
`bundle.windows.signCommand` that calls [`artifact-signing-cli`](https://github.com/levminer/trusted-signing-cli)
(`cargo install artifact-signing-cli`, plus .NET 8, the Azure CLI and the Windows SDK's
signtool). Give all six or none — a partial set fails the build, because the alternative is
shipping an unsigned installer while believing it was signed.

The signing config is deliberately **not** in `tauri.conf.json`: `certificateThumbprint` or
`signCommand` sitting there makes every build on a machine without the credentials fail at
the signing step, which would break local trial builds. It is merged in at build time with
`tauri build --config` instead. Note that no `TAURI_WINDOWS_*` environment variable exists —
Tauri reads Windows signing settings from the config only.

New certificates start with no SmartScreen reputation, so the first few releases may still
warn even when correctly signed; reputation accrues per certificate as downloads add up.

---

## Related documents

The main repository holds a set of spec and design documents; `spec §x.y` references in
code comments point at these:

- `asale-client-spec.md` — client implementation spec (how)
- `asale-client-design.md` — design trade-offs (why)
- `token-trading.md` — the actual buyer → platform → seller path, annotated with code locations
- `deploy/README.md` — deployment, certificates, environment variables and client packaging
