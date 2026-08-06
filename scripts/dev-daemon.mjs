// `pnpm dev:daemon` — the standalone daemon on :9700, for plain-Vite/browser
// (B/S) dev where no Tauri shell is there to spawn one in-process.
//
// Unlike dev:app this keeps the default port and data directory: the frontend
// served by `pnpm dev` talks to 127.0.0.1:9700 (see lib.ts `apiBase`), so
// moving it would just break that path. What it does share with dev:app is the
// stack — a dev daemon must not trade on the live market either.
//
// One Node script for both platforms, so the port check and the four pinned
// variables exist once instead of once per shell.

import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { applyStack, follow, loadEnvFile, portInUse, root } from "./dev-env.mjs";

const PORT = 9700;

if (await portInUse(PORT)) {
  console.log(`asaled already up on :${PORT}`);
  process.exit(0);
}

const env = applyStack(loadEnvFile(), { prod: process.argv.includes("--prod") });

follow(
  spawn("cargo", ["run", "--bin", "asaled"], {
    cwd: resolve(root, "daemon"),
    env,
    stdio: "inherit",
  }),
);
