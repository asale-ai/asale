// `pnpm dev:app` — the Tauri shell with hot reload, pinned to the local stack
// and to its own data directory and ports so it can run beside an installed
// release. Everything it decides lives in dev-env.mjs; this file is the entry
// point. `--prod` aims the same build at the live market on purpose.

import { spawn } from "node:child_process";
import {
  DEV_INSTANCE,
  applyStack,
  describeStack,
  follow,
  loadEnvFile,
  pinAll,
  pnpm,
  root,
} from "./dev-env.mjs";

const prod = process.argv.includes("--prod");
const env = loadEnvFile();

pinAll(env, { ASALE_DATA_DIR: `${env.HOME || env.USERPROFILE}/.asale-dev` });
pinAll(env, DEV_INSTANCE);
applyStack(env, { prod });

if (process.argv.includes("--check")) {
  console.log(`dev:app is ready (${prod ? "prod" : "local"} stack)`);
  for (const key of ["ASALE_DATA_DIR", ...Object.keys(DEV_INSTANCE), "VITE_ASALE_STUDIO", "VITE_ASALE_SWARM", "VITE_ASALE_AEO"]) {
    console.log(`  ${key}=${env[key] ?? "(compiled default)"}`);
  }
  console.log(describeStack(env));
  process.exit(0);
}

const { command, shell } = pnpm();
follow(
  spawn(command, ["tauri", "dev", "--config", "src-tauri/tauri.dev.conf.json"], {
    cwd: root,
    env,
    stdio: "inherit",
    shell,
  }),
);
