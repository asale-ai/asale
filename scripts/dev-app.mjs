import { readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const envFile = resolve(root, ".env");
const shell = { ...process.env };
const env = { ...process.env };

try {
  for (const rawLine of readFileSync(envFile, "utf8").split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const match = line.match(/^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/);
    if (match && env[match[1]] === undefined) {
      env[match[1]] = match[2].replace(/^(['"])(.*)\1$/, "$2");
    }
  }
} catch {
  console.error(`Missing ${envFile}; copy .env.example to .env first.`);
  process.exit(1);
}

// Everything that decides *where this instance lives* — data directory, the two
// ports, the daemon the frontend talks to — is pinned beside the installed
// release so both can run at once. `.env` must not have a say here: it is the
// *packaging* configuration (baked in via option_env!), so its ASALE_PROXY_PORT
// is the port the shipped app will use, and letting it through would make dev
// fight the installed client for it. A real shell variable still wins, which is
// how you run a second dev instance.
const pin = (key, value) => {
  env[key] = shell[key] || value;
};
pin("ASALE_DATA_DIR", `${env.HOME || env.USERPROFILE}/.asale-dev`);
pin("ASALE_BIND", "127.0.0.1:9701");
pin("ASALE_PROXY_PORT", "9788");
pin("VITE_ASALE_DAEMON", "http://127.0.0.1:9701");

// Which *stack* this instance trades against is the same kind of decision, and
// for the same reason it cannot come from `.env`: those endpoints are the ones
// the shipped installer must carry, so inheriting them silently aims every
// `pnpm dev:app` at the live market — real subscription quota, real balances,
// real reputation, from a build nobody released. Development belongs on the
// local stack (asale-server: gateway :9081/:9082, web :9090).
//
// The quota public key has to move with them. It is what a seller checks before
// serving a request, so it is only meaningful paired with the stack that signs
// the grants: the local key against the local gateway, the production key
// against the production one. Mixing the two is not a weaker setup, it is a
// broken one — the client refuses to go on the market and reports being kicked,
// which reads like a server problem and is not one. The value below is the
// public half of asale-server/.env's ASALE_QUOTA_SIG_SEED.
const LOCAL = {
  ASALE_SERVER_API: "http://127.0.0.1:9090",
  ASALE_GATEWAY_API: "http://127.0.0.1:9081",
  ASALE_GATEWAY_WS: "ws://127.0.0.1:9082/v1/ws",
  ASALE_QUOTA_PUBKEY: "oN6MtrUQvzGkFSFZklnzPJkJ5Dlvx5Xu4OK3BovyJ+w=",
};
// `--prod` is the deliberate exception: run the dev build against the real
// stack, taking those four from `.env` exactly as packaging would. Say so
// loudly — the whole point is that it should never happen by accident.
const useProd = process.argv.includes("--prod");
const missing = Object.keys(LOCAL).filter((key) => !env[key]);
if (useProd) {
  if (missing.length) {
    console.error(`--prod needs these in asale-client/.env: ${missing.join(", ")}`);
    process.exit(1);
  }
  console.warn("⚠ dev:app --prod: this build trades on the LIVE market");
} else {
  for (const [key, value] of Object.entries(LOCAL)) pin(key, value);
}

if (process.argv.includes("--check")) {
  console.log(`asale-client/.env is ready for dev:app (${useProd ? "prod" : "local"} stack)`);
  for (const key of ["ASALE_DATA_DIR", "ASALE_BIND", "ASALE_PROXY_PORT", ...Object.keys(LOCAL)]) {
    console.log(`  ${key}=${env[key]}`);
  }
  process.exit(0);
}

// Windows: Node >= 18.20.2 拒绝直接 spawn .cmd/.bat (CVE-2024-27980), 会抛 EINVAL,
// 所以那边必须走 shell。参数里没有空格, 不用额外加引号。
const isWindows = process.platform === "win32";
const command = isWindows ? "pnpm.cmd" : "pnpm";
const child = spawn(
  command,
  ["tauri", "dev", "--config", "src-tauri/tauri.dev.conf.json"],
  { cwd: root, env, stdio: "inherit", shell: isWindows },
);
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 1);
});
