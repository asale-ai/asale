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

OAuth client credentials are documented in [`.env.example`](../.env.example) (Gemini
requires your own; Claude/Codex have public defaults).

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
empty, the Linux webkit2gtk-4.1 dependency, and a warning when the Apple certificate is
missing on macOS.

Tauri cannot cross-compile bundles: `.dmg` only on macOS, `.msi`/`.exe` only on Windows,
`.deb`/`.AppImage` only on Linux. Three platforms = three machines — or push a `v*` tag and
let the three jobs in
[`.github/workflows/release.yml`](../.github/workflows/release.yml) produce all of them at
once.

Output lands in `target/<target>/release/bundle/`, with a `.sig` next to each installer.

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

---

## Related documents

The main repository holds a set of spec and design documents; `spec §x.y` references in
code comments point at these:

- `asale-client-spec.md` — client implementation spec (how)
- `asale-client-design.md` — design trade-offs (why)
- `token-trading.md` — the actual buyer → platform → seller path, annotated with code locations
- `deploy/README.md` — deployment, certificates, environment variables and client packaging
