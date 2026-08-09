### Install

| Platform | File |
| --- | --- |
| macOS (universal, Apple silicon / Intel) | `.dmg` — signed and notarized |
| Windows (x64) | `-setup.exe`, or `.msi` — Authenticode-signed |
| Linux (x64) | `.AppImage` or `.deb` |
| Server / no desktop | `asale-cli-*.tar.gz` — the `asale` CLI plus the `asaled` service |

Or let the installer pick for you. Running the same line again later upgrades in place:

```sh
curl -fsSL https://asale.ai/dl/install.sh | sh   # macOS / Linux
irm https://asale.ai/dl/install.ps1 | iex        # Windows
```

First time here? [What Asale does, and how to start selling or buying](https://github.com/asale-ai/asale#readme).

---
