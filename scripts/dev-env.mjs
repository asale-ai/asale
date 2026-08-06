// The one place that decides what a *development* client points at.
//
// Two entry points share it: `dev-app.mjs` (the Tauri shell) and
// `dev-daemon.mjs` (the standalone daemon behind plain-Vite/browser dev).
//
// The guiding rule here is that nothing gets a second home. The endpoints are
// not listed below because core/src/config.rs already defaults to the local
// stack; this file removes the packaged values instead of restating them. The
// quota public key is derived from the seed the local gateway signs with,
// rather than pasted next to it.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { createConnection } from "node:net";
import { createPrivateKey, createPublicKey } from "node:crypto";

export const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/** The four values that name a stack: three endpoints plus the key that proves
 *  the gateway on the other end is allowed to spend a seller's quota. They
 *  travel together — see `applyStack`. */
const STACK_KEYS = [
  "ASALE_SERVER_API",
  "ASALE_GATEWAY_API",
  "ASALE_GATEWAY_WS",
  "ASALE_QUOTA_PUBKEY",
];

/** `.env.package` is *packaging* configuration: the values baked into a release
 *  (via option_env!) because a double-clicked app has no shell environment. It
 *  therefore holds production endpoints, which is exactly why development must
 *  not inherit them — see `applyStack`. What it is read for here is the rest:
 *  OAuth client ids, signing passphrases. */
export function loadEnvFile() {
  const envFile = resolve(root, ".env.package");
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
    console.error(`Missing ${envFile}; copy .env.package.example to it first.`);
    process.exit(1);
  }
  return env;
}

/**
 * The local gateway's quota public key, derived from the seed it signs with
 * (asale-server/.env, ASALE_QUOTA_SIG_SEED).
 *
 * Derived rather than written down, because the two halves failing apart is not
 * a visible error: the client quietly refuses to go on the market and reports
 * being kicked, which reads like a server fault and is not one. Rotate the
 * seed and this follows on the next run.
 *
 * Returns null when asale-server is not checked out beside this repo — the
 * client is published on its own, and a contributor with only this half should
 * still get a running app, just one that cannot sell.
 */
function localQuotaPubkey() {
  const seedFile = resolve(root, "..", "asale-server", ".env");
  let seed;
  try {
    const text = readFileSync(seedFile, "utf8");
    seed = text.match(/^\s*ASALE_QUOTA_SIG_SEED=(.*)$/m)?.[1]?.trim().replace(/^(['"])(.*)\1$/, "$2");
  } catch {
    return null;
  }
  if (!seed) return null;
  const raw = Buffer.from(seed, "base64");
  if (raw.length !== 32) {
    console.warn(`⚠ ASALE_QUOTA_SIG_SEED in ${seedFile} is not 32 bytes — ignoring it`);
    return null;
  }
  // Ed25519 PKCS#8 is a fixed 16-byte prefix followed by the raw seed; SPKI
  // likewise ends with the raw public key. Node has no direct raw-key import.
  const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), raw]);
  const priv = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
  const spki = createPublicKey(priv).export({ format: "der", type: "spki" });
  return spki.subarray(-32).toString("base64");
}

/** Where a `pnpm dev:app` instance lives — beside the installed release rather
 *  than on top of it, so both can run at once. These have no other source:
 *  `.env.package`'s ASALE_PROXY_PORT is the *shipped* app's port, and letting
 *  it through would make dev fight the installed client for it. */
export const DEV_INSTANCE = {
  ASALE_BIND: "127.0.0.1:9701",
  ASALE_PROXY_PORT: "9788",
  VITE_ASALE_DAEMON: "http://127.0.0.1:9701",
};

/**
 * Apply `pins` to `env`, letting a real shell variable win — that is how you
 * run a second dev instance, or aim one somewhere else for an afternoon.
 */
export function pinAll(env, pins) {
  const shell = process.env;
  for (const [key, value] of Object.entries(pins)) {
    env[key] = shell[key] || value;
  }
  return env;
}

/**
 * Point `env` at a stack.
 *
 * Development drops the three endpoints instead of setting them: unset means
 * `option_env!` sees nothing, and core/src/config.rs already defaults to the
 * local stack (:9090 / :9081 / :9082). Restating them here would be a second
 * copy of a decision that already has an owner.
 *
 * The key cannot work that way — there is no sensible default for "which
 * gateway may spend your subscription" — so it is derived from the local
 * seed. Local key ↔ local gateway, production key ↔ production gateway; mixing
 * the two is not a weaker setup, it is a broken one.
 *
 * `--prod` is the deliberate exception: run a dev build against the real stack,
 * taking all four from `.env.package` exactly as packaging would.
 */
export function applyStack(env, { prod }) {
  if (prod) {
    const missing = STACK_KEYS.filter((key) => !env[key]);
    if (missing.length) {
      console.error(`--prod needs these in asale-client/.env.package: ${missing.join(", ")}`);
      process.exit(1);
    }
    console.warn("⚠ --prod: this build trades on the LIVE market");
    return env;
  }
  for (const key of STACK_KEYS) delete env[key];
  const pubkey = localQuotaPubkey();
  if (pubkey) {
    pinAll(env, { ASALE_QUOTA_PUBKEY: pubkey });
  } else {
    console.warn(
      "⚠ no asale-server/.env beside this repo — starting without a quota public key.\n" +
        "  Buying works; selling is off, because the client cannot tell whether the\n" +
        "  gateway it reaches is allowed to spend your subscription.",
    );
  }
  return env;
}

/** What this run resolved to, for `--check` and for the banner. Endpoints read
 *  "(compiled default)" when they were dropped — that is the point. */
export function describeStack(env) {
  return STACK_KEYS.map((key) => `  ${key}=${env[key] ?? "(compiled default)"}`).join("\n");
}

/** Whether something already answers on a loopback port. Node's own check, so
 *  the daemon task needs no `nc` on macOS and no Test-Port on Windows. */
export function portInUse(port) {
  return new Promise((res) => {
    const sock = createConnection({ host: "127.0.0.1", port });
    const done = (v) => {
      sock.destroy();
      res(v);
    };
    sock.once("connect", () => done(true));
    sock.once("error", () => done(false));
    sock.setTimeout(1000, () => done(false));
  });
}

/** Windows: Node >= 18.20.2 refuses to spawn .cmd/.bat directly
 *  (CVE-2024-27980) and throws EINVAL, so that side has to go through a shell.
 *  No argument here contains a space, so nothing needs extra quoting. */
export function pnpm() {
  const isWindows = process.platform === "win32";
  return { command: isWindows ? "pnpm.cmd" : "pnpm", shell: isWindows };
}

/** Forward the child's exit — including the signal — as our own. */
export function follow(child) {
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exit(code ?? 1);
  });
}
