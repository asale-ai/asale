### Install

| Platform | File |
| --- | --- |
| macOS (universal, Apple silicon / Intel) | `.dmg` — signed and notarized |
| Windows (x64) | `-setup.exe`, or `.msi` — Authenticode-signed |
| Linux (x64) | `.AppImage` or `.deb` — Ubuntu 22.04 / Debian 12 or newer (glibc 2.35) |
| Server / no desktop | `asale-cli-*.tar.gz` — the `asale` CLI plus the `asaled` service; on Linux, any distribution with glibc 2.17 or newer |

Or let the installer pick for you. Running the same line again later upgrades in place:

```sh
# macOS / Linux
curl -fsSL https://asale.ai/dl/install.sh | sh
# Windows
irm https://asale.ai/dl/install.ps1 | iex
```

First time here? [What Asale does, and how to start selling or buying](https://github.com/asale-ai/asale#readme).

---
