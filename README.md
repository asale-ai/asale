# asale client

Desktop client for the asale token-exchange platform. Per `../asale-client-spec.md`:
**Tauri 2 + React** GUI wrapping a Rust core.

## Layout

- `core/` — **`asale-client-core`**, the non-GUI Rust core (compiles + tested standalone):
  - `protocol.rs` — wsrelay frame envelope + types (mirrors the server; the server
    e2e test already exercises a publisher speaking these exact frames).
  - `security.rs` — device Ed25519 identity; signs the WS handshake
    `device_id|ts|nonce` exactly as the server verifies. Cross-checked by a unit test.
  - `ws.rs` — wsrelay client: signed handshake, `hello`, `supply.declare`,
    30s heartbeat, dispatches inbound `http_request` → executor.
  - `executor.rs` — provider executor (spec §5.2): injects the local subscription
    token (only place it is used), streams the upstream response back as
    `stream_start/chunk/end`, parses usage (Claude/OpenAI/Gemini), enforces budget.
  - `discovery.rs` — `ToolAdapter` trait + provider enum + Claude/ClaudeWork adapter.
  - `store.rs` — SQLite (spec §8): keychain refs only, never plaintext credentials.
  - `config.rs` — server/gateway/proxy endpoints.

Run: `cd core && cargo test`.

## Desktop engineering (spec §12)

Registered Tauri v2 plugins (see `src-tauri/src/lib.rs`; single-instance is
deliberately first in the builder chain):

- **single-instance** — a second launch focuses the running window instead.
- **window-state** — remembers window position/size (visibility excluded so the
  app never starts silently hidden).
- **tray** (`tauri` feature `tray-icon`, `src-tauri/src/tray.rs`) — show/hide
  window, publish on/off (mirrors the live WS session state, reuses the
  `publish_toggle` logic), quit. Closing the window hides to the tray.
- **autostart** — launch-at-login toggle in the Settings page (macOS LaunchAgent).
- **updater + process** — "Check for updates" in Settings: check → download →
  install → relaunch.
- **deep-link** — registers the `asale://` scheme (macOS Info.plist is generated
  from `tauri.conf.json > plugins.deep-link` at bundle time). OAuth keeps the
  loopback callback as the primary route; a deep link such as
  `asale://oauth/callback` focuses the window and is emitted to the webview as
  the `deep-link` event.

### Updater deployment (required for real updates)

`tauri.conf.json` points the updater at:

```
https://dl.asale.app/updater/{{target}}/{{current_version}}
```

This endpoint **must be deployed** before updates work: it has to answer with
the standard Tauri updater JSON (`version`, `pub_date`, `url`, `signature`,
`notes`), returning HTTP 204 when the client is already current. `{{target}}`
and `{{current_version}}` are substituted by the updater at request time.

Update artifacts are produced by `tauri build` (`bundle.createUpdaterArtifacts`
is enabled) and must be signed with the minisign private key kept **outside**
this repo's client tree at `../deploy/asale-updater.key` (generated with
`tauri signer generate`, empty password; the matching public key is embedded in
`tauri.conf.json > plugins.updater.pubkey`). Sign at build time via:

```
TAURI_SIGNING_PRIVATE_KEY_PATH=../deploy/asale-updater.key pnpm tauri build
```

If the private key is ever lost or rotated, shipped clients can no longer
verify new releases — treat `deploy/asale-updater.key` as a production secret
(move it into a proper secret store for CI; do not commit it).

## Remaining (Tauri GUI + flows)

The Rust core is the load-bearing, testable part and is done. Still to build:
- `src-tauri/` — Tauri 2 shell: commands/events wiring the core, OS keychain
  (`keyring` crate) for tokens + device seed, tray/autostart/updater.
- OAuth browser flows per provider (claude/codex/gemini/kimi/xai) — the adapters'
  `login()`/`refresh()` (spec §3.2).
- Consumer local proxy (`axum` on 127.0.0.1:8787) with direct/market routing (spec §6).
- React UI (dashboard/publish/consume/wallet/records/settings) + i18n.

The core already speaks the exact protocol the running server accepts, so wiring the
GUI is mechanical relative to the protocol/crypto work that's complete.
