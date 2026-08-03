import { readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const envFile = resolve(root, ".env");
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

if (!env.ASALE_QUOTA_PUBKEY) {
  console.error("ASALE_QUOTA_PUBKEY is required in asale-client/.env");
  process.exit(1);
}

env.ASALE_DATA_DIR ||= `${env.HOME || env.USERPROFILE}/.asale-dev`;
env.ASALE_BIND ||= "127.0.0.1:9701";
env.ASALE_PROXY_PORT ||= "9788";
env.VITE_ASALE_DAEMON ||= "http://127.0.0.1:9701";

if (process.argv.includes("--check")) {
  console.log("asale-client/.env is ready for dev:app");
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
