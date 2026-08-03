// HTTP RPC client for the asale daemon (asaled).
//
// Every command is served by the Rust daemon — the real native + server logic
// — over `POST /rpc/{cmd}`. The same API backs all frontends:
//   - the Tauri desktop shell (webview → http://127.0.0.1:9700),
//   - Chrome on the same machine (Vite dev server or the daemon-served UI),
//   - a remote browser against a headless box (`asaled --bind 0.0.0.0:9700`,
//     authenticated with the daemon token from the startup log).

// True only inside the actual Tauri shell (used for shell-only features:
// autostart, self-updater, and letting the daemon open the OAuth browser).
const realTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// A backend is always reachable over HTTP now; kept for the pages' guards.
const inTauri = true;

// Server response shapes live in ../../shared/api-types.ts so asale-web
// describes the same JSON with the same types. Re-exported so pages keep
// importing them from "./lib".
export type {
  DepositSession,
  DepositSessionStatus,
  DepositSessionTx,
  Globe,
  GlobeFlow,
  GlobeRegion,
  MarketModel,
  MarketModelPrice,
  OAuthAccount,
  OAuthProvider,
  Profile,
} from "@shared/api-types";

const TOKEN_KEY = "asale.daemon.token";

// Capture `#token=…` into localStorage on first load, then scrub it from the
// address bar. The legacy query form remains readable for old bookmarks, but
// new links use a fragment so the credential never reaches HTTP logs/Referer.
//
// The daemon requires this token on every RPC, loopback included, so a browser
// always needs it — open the URL `asaled` prints at startup. (The Tauri shell
// reads `~/.asale/daemon.token` itself and seeds this same key before any page
// script runs, so the desktop app never shows a token in a URL.)
if (typeof window !== "undefined") {
  const params = new URLSearchParams(window.location.search);
  const fragment = new URLSearchParams(window.location.hash.replace(/^#/, ""));
  const tok = fragment.get("token") || params.get("token");
  if (tok) {
    localStorage.setItem(TOKEN_KEY, tok);
    params.delete("token");
    fragment.delete("token");
    const qs = params.toString();
    const hash = fragment.toString();
    window.history.replaceState(
      null,
      "",
      window.location.pathname + (qs ? `?${qs}` : "") + (hash ? `#${hash}` : "")
    );
  }
}

/** The daemon access token this frontend is using, if it has one.
 *
 *  Exposed so the Settings page can build the URL that opens this same daemon
 *  in another browser — the token is the whole authorization, so a link without
 *  it loads the app and then fails every call it makes. */
export function daemonToken(): string {
  return (typeof window !== "undefined" && localStorage.getItem(TOKEN_KEY)) || "";
}

/** Base URL of the daemon RPC API. */
export function apiBase(): string {
  const env = import.meta.env.VITE_ASALE_DAEMON as string | undefined;
  if (env) return env;
  // Vite dev server / Tauri shell: the daemon listens on its default port.
  if (import.meta.env.DEV || realTauri) return "http://127.0.0.1:9700";
  // Production B/S: the daemon itself serves this page — same origin.
  return "";
}

/**
 * A failed RPC, carrying what the UI needs to say why *in the user's language*.
 *
 * The daemon cannot translate its own failures — one binary serves the desktop
 * shell and any browser, while the chosen locale lives here in i18next. So it
 * sends a catalog `key` plus the values that key interpolates
 * (`daemon/src/commands/mod.rs`, `CmdError`), and errors that originated at the
 * asale server arrive with the server's own key intact. `message` is the
 * English fallback for keys this build has no translation for.
 */
export class DaemonError extends Error {
  /** Message-catalog key, e.g. `errors.wallet.insufficientBalance`; "" when the
   *  daemon had no entry for this failure and `message` is all there is. */
  key: string;
  /** Values the catalog entry interpolates. */
  params: Record<string, string | number | boolean>;
  constructor(
    message: string,
    key = "",
    params: Record<string, string | number | boolean> = {}
  ) {
    super(message);
    this.name = "DaemonError";
    this.key = key;
    this.params = params;
  }
}

/** The daemon did not answer at all (socket refused / no route).
 *
 *  Distinct from a daemon that answered with an error, because the two mean
 *  opposite things at startup: the desktop shell boots its daemon in a
 *  background thread while the webview is already painting, so the first few
 *  RPCs of a perfectly healthy launch fail this way. Callers use this to hold a
 *  loading state instead of flashing "not connected" / "signed out". */
export class DaemonUnreachable extends DaemonError {
  constructor() {
    super("Asale daemon unreachable (asaled not running?)", "errors.daemon.unreachable");
    this.name = "DaemonUnreachable";
  }
}
export const isDaemonDown = (e: unknown) => e instanceof DaemonUnreachable;

/** Resolve once the daemon answers anything, or `false` after `timeoutMs`.
 *
 *  Used as the app's boot gate: pages mount only after this settles, so no page
 *  ever renders its "daemon down" / "signed out" branch just because the daemon
 *  was still coming up. */
export async function waitForDaemon(timeoutMs = 4000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      await invoke("client_status");
      return true;
    } catch (e) {
      // An error response is still an answer — the daemon is up.
      if (!isDaemonDown(e)) return true;
      if (Date.now() >= deadline) return false;
      await new Promise((r) => setTimeout(r, 250));
    }
  }
}

/**
 * Read the daemon's failure envelope — `{error, key?, params?}` — into a
 * `DaemonError`. Also used for the async OAuth flow, which reports its failure
 * in the same shape inside a 200 response.
 */
export function toDaemonError(body: unknown, fallback: string): DaemonError {
  const v = (body ?? {}) as { error?: unknown; key?: unknown; params?: unknown };
  const message = typeof v.error === "string" && v.error ? v.error : fallback;
  return new DaemonError(
    message,
    typeof v.key === "string" ? v.key : "",
    (v.params as Record<string, string | number | boolean>) ?? {}
  );
}

export async function invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  const tok = localStorage.getItem(TOKEN_KEY);
  if (tok) headers["x-asale-token"] = tok;

  let resp: Response;
  try {
    resp = await fetch(`${apiBase()}/rpc/${cmd}`, {
      method: "POST",
      headers,
      body: JSON.stringify(args ?? {}),
    });
  } catch {
    throw new DaemonUnreachable();
  }
  const v = await resp.json().catch(() => null);
  if (!resp.ok) {
    throw toDaemonError(v, `HTTP ${resp.status}`);
  }
  return v as T;
}

// ── Browser OAuth (two-step: begin → open URL → poll result) ───────────────
export interface OAuthFlowStart {
  flow_id: string;
  auth_url: string;
  provider: string;
  /** Device-code flows only: the short string the user confirms on screen. */
  user_code?: string;
  /** Device-code flows only: seconds until that code stops being accepted. */
  expires_in?: number;
}

/**
 * Run a browser OAuth flow end to end. The daemon starts the loopback
 * callback and returns the authorize URL; in the desktop shell the daemon
 * opens the system browser itself (`openLocal`), in a plain browser we open
 * a new tab. Then poll until the callback + token exchange finish.
 */
export async function runOAuthFlow<T = unknown>(
  cmd: "oauth_login" | "oauth_device_login" | "platform_oauth_login",
  args: Record<string, unknown>,
  /** Called once the flow has started, before polling begins. A device-code
   *  flow has to show `user_code` for the whole wait — the user reads it off
   *  this screen and confirms it in the browser. */
  onStart?: (start: OAuthFlowStart) => void,
): Promise<T> {
  const start = await invoke<OAuthFlowStart>(cmd, { ...args, openLocal: realTauri });
  onStart?.(start);
  if (!realTauri) {
    window.open(start.auth_url, "_blank", "noopener");
  }
  const deadline = Date.now() + 300_000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 2000));
    // A flow that failed after the browser handed control back reports it in
    // the RPC *body* (status 200), so the envelope is unwrapped by hand — but
    // it carries the same key/params a synchronous failure would.
    const res = await invoke<{ status: string; result?: T }>("oauth_result", {
      flowId: start.flow_id,
    });
    if (res.status === "ok") return res.result as T;
    if (res.status === "error") throw toDaemonError(res, "authorization failed");
  }
  throw new DaemonError("authorization timed out", "errors.oauth.timedOut");
}

export interface ClientConfig {
  server_api_base: string;
  gateway_api_base: string;
  gateway_ws_url: string;
  proxy_port: number;
  device_id: string;
}
export interface Wallet {
  balance: number;
  frozen: number;
  payable: number;
  unit: string;
}
/** One on-chain deposit seen for this account. */
export interface DepositRow {
  id: number;
  chain: string;
  tx_hash: string;
  /** What landed on chain. The wallet was credited `amount - fee`. */
  amount: number;
  fee: number;
  confirmations: number | null;
  /** 1 seen · 2 confirmed · 3 credited. */
  status: number;
  created_ts: number;
}
/** One withdrawal request of this account. */
export interface WithdrawalRow {
  id: number;
  chain: string;
  to_address: string;
  amount: number;
  fee: number | null;
  tx_hash: string | null;
  /** 1 requested · 2 risk_review · 3 broadcast · 4 confirmed · 5 rejected. */
  status: number;
  requested_ts: number;
  confirmed_ts: number | null;
}
/** Server-side rules for one funding rail, surfaced so the form states them up
 *  front. Fees and floors differ per chain (an SPL transfer costs a fraction of
 *  a TRC20 one); the AML limits are the user's and hold across all rails. */
export interface WithdrawLimits {
  /** "solana" | "tron" — which rail these numbers belong to. */
  chain: string;
  /** Flat fee carved out at credit time; the wallet gains `deposited - deposit_fee`. */
  deposit_fee: number;
  /** Advisory floor shown in the UI — smaller deposits are still credited. */
  deposit_min: number;
  withdraw_min: number;
  /** Flat fee carved out of the requested amount; the destination receives
   *  `amount - withdraw_fee`. */
  withdraw_fee: number;
  withdraw_max_single: number;
  withdraw_max_daily: number;
  withdraw_max_per_hour: number;
  whitelist_only: boolean;
}
export interface WalletHistory {
  deposits: DepositRow[];
  withdrawals: WithdrawalRow[];
  /** Which rail the wallet dialog opens on. */
  default_chain: string;
  /** Every rail the platform takes money on. */
  chains: WithdrawLimits[];
  /** The default rail's schedule, flat, for callers that want just one. */
  limits: WithdrawLimits;
}

/** Base58 alphabet Solana (and Bitcoin) use — no 0, O, I or l. */
const B58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/** True if `s` decodes to exactly 32 bytes, i.e. a Solana address.
 *
 *  A character-class regex is not enough here: a TRON address is also valid
 *  base58 of a plausible length, so "T…" pasted into the Solana form would pass
 *  a pattern check and send funds to an account with no key. Only the decoded
 *  length separates them. */
export function isSolanaAddress(s: string): boolean {
  const str = s.trim();
  if (!str) return false;
  // Base-58 to base-256, little-endian, by hand: BigInt is past this build's
  // target and a 32-byte number does not fit a double.
  const bytes: number[] = [];
  for (const ch of str) {
    const v = B58_ALPHABET.indexOf(ch);
    if (v < 0) return false;
    let carry = v;
    for (let i = 0; i < bytes.length; i++) {
      carry += bytes[i] * 58;
      bytes[i] = carry & 0xff;
      carry >>>= 8;
    }
    while (carry > 0) {
      bytes.push(carry & 0xff);
      carry >>>= 8;
    }
  }
  // Leading '1's are leading zero bytes and carry no magnitude of their own.
  let zeros = 0;
  while (zeros < str.length && str[zeros] === "1") zeros++;
  return zeros + bytes.length === 32;
}
export type PublishState = "offline" | "connecting" | "reconnecting" | "online" | "throttled" | "kicked";

export interface ProxyStatus {
  port: number;
  api_key_loaded: boolean;
  publishing: boolean;
  /** Live WS session state. */
  publish_state: PublishState;
  /** Whether this device should be on the market at all — derived from the
   *  per-account sell switches, not a switch of its own. */
  publish_wanted: boolean;
}

/** Everything the always-on status widget shows, in one poll (`client_status`).
 *
 *  The widget renders on every page, so this deliberately answers the whole
 *  question at once — link state, plus whether being linked is currently worth
 *  anything (signed in, accounts selling, lanes actually serving). */
export interface ClientStatus {
  publish_state: PublishState;
  signed_in: boolean;
  api_key_loaded: boolean;
  proxy_port: number;
  device_id: string;
  server_api_base: string;
  gateway_ws_url: string;
  /** Connected subscription accounts, and those with the sell switch on. */
  accounts_total: number;
  selling: { provider: string; account_id: string }[];
  /** `(account, model)` lanes serving right now, and lanes of a selling
   *  account that are not (cooling, paused, exhausted). */
  lanes_selling: number;
  lanes_blocked: number;
  /** Labels of the CLIs currently buying through the local proxy. */
  buying: string[];
  version: string;
}

export const fmtUsdt = (micros: number) =>
  (micros / 1_000_000).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 6 });

/** Compact token count: 1.2M / 34k / 900. */
export const fmtTokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`
    : n >= 1000 ? `${Math.round(n / 1000)}k`
    : String(Math.round(n));

// ── CLI import (spec §3.3) ──
export interface DiscoveredCred {
  provider: string;
  source: string;
  has_refresh_token: boolean;
  expires_at: number | null;
  expired: boolean;
  /** Digest of the credential's refresh token: two sources sharing it are the
   *  same subscription account (keychain + file), not two logins. */
  fingerprint: string;
  account_hint: string | null;
  plan: string | null;
}
/** One imported *subscription account* — sources holding the same account are
 *  merged into a single entry before this is returned. */
export interface ImportResult {
  provider: string;
  account_id: string;
  /** The store the imported credential was actually read from. */
  source: string;
  /** Every store found holding this same account, chosen one first. */
  sources: string[];
  plan: string | null;
  expires_at: number | null;
  has_refresh_token: boolean;
}
/** A local CLI left out of the import because it is buying: its config points at
 *  the asale proxy, so nothing in its directory is this device's to sell.
 *  `dropped` lists accounts already imported from it that were retired. */
export interface ImportSkipped {
  provider: string;
  label: string;
  reason: "buying";
  dropped: string[];
}
/** Result of the bulk local-CLI import (runs at daemon startup + refresh button). */
export interface ImportAllResult {
  imported: ImportResult[];
  warnings: string[];
  skipped: ImportSkipped[];
  errors: { provider: string; error: string }[];
}

// ── Account pool (spec §4) — one row per connected subscription account ──
/** One `(account, model)` lane and why it is or is not selling.
 *
 *  Availability is per model, not per account: an account's Opus window can be
 *  spent while its Haiku lane is wide open, so the sell page needs this to say
 *  anything more useful than "connected". */
export interface Lane {
  provider: string;
  account_id: string;
  model: string;
  status: "selling" | "cooldown" | "paused" | "withheld" | "off" | "expired" | "exhausted";
  /** `rate_limit` | `quota` | `auth` | `breaker` | `manual` when paused,
   *  `price` when the market moved outside the account's price band. */
  paused_reason: string | null;
  /** True when the operator has to fix something and press resume. */
  requires_user: boolean;
  /** Unix seconds it comes back on its own; 0 = it needs the operator. */
  resume_at: number;
  cooldown_until: number | null;
  fail_streak: number;
  last_error: string;
  sell_enabled: boolean;
  quota_remaining: number;
  /** What the market currently pays for this model, as whole percent *of* the
   *  vendor's list price (100 = list price). `null` when this device has not
   *  read a price for it yet, which is not a price of zero and must not be
   *  drawn as one. */
  ratio: number | null;
  /** The account's own band, carried on the lane so the chart can draw the
   *  threshold next to the bar it explains. */
  min_ratio: number;
  max_ratio: number;
}

/** One custom endpoint (internal): an OpenAI-compatible base URL sold as if it
 *  were a subscription. `sellable_models` is the part of what the endpoint
 *  serves that the platform actually trades — an endpoint can offer hundreds of
 *  models and share none with the catalog, which is the one thing that explains
 *  a connected endpoint earning nothing. */
export interface CustomEndpoint {
  account_id: string;
  base_url: string;
  sell_enabled: boolean;
  /** Price floor, whole percent *of* list price (see `AccountStatus`). */
  min_ratio: number;
  /** Requests served at once, declared to the market as the lane's ceiling. */
  concurrency: number;
  sellable_models: string[];
}

export interface AccountStatus {
  provider: string;
  account_id: string;
  plan: string | null;
  quota_remaining: number;
  status: "available" | "cooldown" | "expired" | "exhausted";
  expires_at: number | null;
  cooldown_until: number | null;
  /** This account's own sell switch (selling is per account, not per provider). */
  sell_enabled: boolean;
  /** This account's daily sell cap in tokens; 0 = unlimited. */
  sell_daily_limit: number;
  /** The price floor this account sells at, in whole percent *of* list price: a
   *  model the market prices below it is withheld until it comes back. `10` —
   *  the default, and below every price the server can quote — never withholds
   *  anything.
   *
   *  `sell_max_ratio` is the band's other end, which the UI no longer sets: no
   *  price is too good to accept, so it stays at list price. */
  sell_min_ratio: number;
  sell_max_ratio: number;
  /** How many requests this account serves at once. Declared to the market as
   *  the lane's concurrency ceiling, so the gateway stops routing work here
   *  past it rather than this device refusing what it was already sent. */
  sell_concurrency: number;
  /** Tokens this account served today / in the current 5h window. */
  used_today: number;
  used_window: number;
  /** Its plan's 5h cap and the daily equivalent (×24/5), for "% of plan". */
  window_cap: number;
  daily_cap: number;
  /** A metered platform key rather than a plan subscription: billed against a
   *  balance, so it has no rolling window and the two caps above are guesses
   *  that do not apply to it. The UI hides window figures for these. */
  key_based: boolean;
  /** `oauth` = asale owns this login outright; `import` = copied from a local CLI. */
  origin: string | null;
  /** Where the credential in use came from (keychain service or file path). */
  source: string | null;
  /** Every local store holding this same subscription account. More than one
   *  entry means those stores were merged into this single record — selling is
   *  per subscription account, so one account is always one row. */
  sources: string[];
  /** True when the local CLI uses this same account — refreshing can rotate
   *  the other side's token out, so the UI warns before selling it. */
  shared_with_local_cli: boolean;
}

// ── Consume mode (spec §6.1) ──
export type ConsumeMode = "direct" | "market" | "auto";
export interface ConsumeModeInfo {
  mode: ConsumeMode;
  direct_available: { claude: boolean; gemini: boolean };
}

// ── Buy side: one switch + model multi-select per installed CLI (flow §4/§6) ──
export type BuyToolId = "claude" | "codex" | "gemini";
export interface BuyTool {
  id: BuyToolId;
  label: string;
  /** Is this CLI present on the machine? */
  installed: boolean;
  /** The subscription account the CLI is locally signed in as (identity only). */
  account: string | null;
  plan: string | null;
  config_path: string;
  config_paths: string[];
  current_base_url: string | null;
  /** Buy switch for this tool. */
  enabled: boolean;
  /** True when the tool's live config really points at the asale proxy. */
  in_effect: boolean;
  /** Models this tool may buy; empty = any model the market offers. */
  models: string[];
  since: number;
}
export interface BuyTools {
  tools: BuyTool[];
  proxy_base: string;
}
/** Market prices are micro-USDT per 1K tokens; bill sheets quote per 1M. */
export const pricePerMillion = (micros: number) => micros / 1000;

/** Compact context window: 1M / 200K / 8192. */
export const fmtContext = (n: number) =>
  n >= 1_000_000 ? `${+(n / 1_000_000).toFixed(1)}M`
    : n >= 10_000 ? `${Math.round(n / 1000)}K`
    : String(n);

export interface SetBuyToolResult {
  tool: BuyToolId;
  enabled: boolean;
  base_url?: string;
  config_paths?: string[];
  backed_up?: boolean;
  models?: string[];
  restored?: boolean;
}

// ── Usage limits (rate-limit windows per provider, Limits page) ──
export interface LimitWindow {
  key: string;
  label: string;
  used_percent: number;
  reset_at: number | string | null;   // unix seconds, or ISO string (live Claude)
  window_seconds: number;
}
export interface LimitProvider {
  id: string;
  connected: boolean;
  plan_label?: string | null;
  windows?: LimitWindow[];
  live?: boolean;   // true when windows came from the provider's live API
  /** Why `live` is false, when a live read was attempted and failed. Absent for
   *  providers that expose no usage endpoint at all. */
  fallback_reason?: string | null;
}
export interface UsageLimits {
  providers: LimitProvider[];
}

// ── Usage summary (three categories) ──
export interface UsageBucket {
  tokens: number;
  amount_usdt: number;
  count: number;
}
export interface UsageSummary {
  period: "day" | "week" | "month" | "all";
  sold: UsageBucket;
  bought: UsageBucket;
}

// ── Usage overview (full dashboard, Usage page) ──
export type UsageScope = "used" | "bought" | "sold";
export type UsagePeriod = "day" | "week" | "month" | "total";
export interface UsageModelRow { model: string; tokens: number; count: number; share: number }
export interface UsageDailyRow { date: string; total: number; input: number; output: number; cache: number; count: number }
export interface UsageHeatCell { date: string; tokens: number }
export interface UsageOverview {
  period: UsagePeriod;
  scope: UsageScope;
  total_tokens: number;
  total_amount: number;   // micro-USDT
  conversations: number;
  models: UsageModelRow[];
  daily: UsageDailyRow[];
  heatmap: UsageHeatCell[];
  stats: { d7: number; d30: number; avg: number; active_days: number; first_day: string | null };
}

// ── Records (spec §8) ──
export interface ServerRecord {
  task_id: string;
  model: string;
  provider: string;
  in_tokens: number;
  out_tokens: number;
  amount_usdt: number;
  provider_income: number;
  platform_fee: number;
  status: number;
  created_ts: number;
}
export interface LocalRecord {
  task_id: string;
  ts: number;
  model: string;
  in_tokens: number;
  out_tokens: number;
  amount_usdt: number;
  status: string;
}
export interface RecordsPage {
  role: string;
  page: number;
  per_page: number;
  records: ServerRecord[];
  server_error: string | null;
  local: { records: LocalRecord[]; total: number };
}
/** Shape of `reconcile_now`. The daemon still reconciles in the background;
 *  the records page no longer exposes a manual trigger for it. */
export interface ReconcileResult {
  provider: {
    matched: number;
    local_only: number;
    server_only: number;
    token_mismatch: number;
    amount_mismatch: number;
  };
  synced_amounts: number;
  consumer: { server_page_count: number; local_total: number };
  reconciled_at: number;
}

export { inTauri, realTauri };
