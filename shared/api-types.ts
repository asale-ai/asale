// Shapes the asale server returns, shared by both frontends.
//
// The desktop client (`asale-client`, Vite + React 18) and the web console
// (`asale-web`, Next.js + React 19) each used to declare these independently,
// and they had already drifted apart while describing the very same JSON:
//
//   - `region` was `string | null` in one and `string` in the other,
//   - `role` existed only in the web copy, so the client could not read it,
//   - `created_at` was `string` in one and `string | number` in the other,
//   - `OAuthAccount.provider` was a bare `string` in one, a union in the other.
//
// Only one of each pair could be right. These declarations are checked against
// the handlers that produce them (`api/profile.rs`, `api/market.rs`) — see the
// per-field notes where a frontend's old guess was wrong.
//
// Types only: everything here is erased at build time, so neither bundler ever
// resolves this file — it costs nothing at runtime and needs no package
// manager workspace. Runtime code stays in each app; the two use different
// i18n libraries, React majors and styling systems, so components are not
// shareable without a real design-system effort.

/** Platform identity providers. */
export type OAuthProvider = "google" | "github";

export interface OAuthAccount {
  provider: OAuthProvider;
  email: string;
}

/** `GET /api/v1/me/profile`. */
export interface Profile {
  user_id: number;
  email: string;
  /** `users.name`, a non-null column — empty string when never set, not null. */
  name: string;
  /** `users.avatar_url`, likewise non-null and possibly empty. */
  avatar_url: string;
  /** `users.region`, likewise non-null and possibly empty. */
  region: string;
  kyc_level: number;
  status: number;
  /** `user` | `admin` — gates the admin center link. */
  role: string;
  /** ISO-8601 UTC, formatted server-side (`YYYY-MM-DDTHH:MM:SSZ`). */
  created_at: string;
  has_password: boolean;
  oauth_accounts: OAuthAccount[];
}

/** One token class's pricing for a model. Amounts are micro-USDT per 1k tokens. */
export interface MarketModelPrice {
  /** `input` | `output` | `cache_read` | `cache_write`. */
  token_type: string;
  ref_price: number;
  market_price: number;
  discount: number;
}

/** One row of `GET /api/v1/market/models`. */
export interface MarketModel {
  /** Bare model id, as it appears in a request body. */
  model: string;
  /** Catalog id, `vendor/model` (the platform stores the two halves apart). */
  model_id: string;
  provider: string;
  display_name: string;
  context_length: number;
  modality: string;
  prices: MarketModelPrice[];
  /** Market price as a fraction of the vendor's list price, in [0.1, 1.0]. */
  ratio: number;
  /** `1 - ratio`, i.e. how far below list the model is trading. */
  discount: number;
  supply_capacity_tokens: number;
  /** Online sell-side subscriptions offering this model right now. */
  online_lanes: number;
  /** Buy-side calls in the last complete minute. */
  calls_last_minute: number;
  /** Alias of `calls_last_minute`, kept for older clients. */
  demand: number;
  /**
   * How much of this model's live supply has proved it serves this model.
   *
   * A pair, not a boolean, and both `null` when the platform has no reading —
   * a model with no supply, or a deployment with verification switched off.
   * "3 of 5 checked" is something a buyer can weigh; a green tick over a
   * market where one lane in five passed would be a claim the platform cannot
   * support, and a red one would accuse sellers nobody has got to yet.
   *
   * Aggregate by design. Which *lanes* failed is not on a public endpoint:
   * that list is a directory of who is under suspicion, and it is nobody's
   * business but the seller's and the operator's.
   */
  verified_lanes: number | null;
  declared_lanes: number | null;
}

/**
 * Third-party scores for one model, from Artificial Analysis
 * (`asale-server/src/benchmarks.rs`).
 *
 * Every field is nullable and means it: AA does not run every evaluation
 * against every model, and it only measures serving performance for models it
 * currently probes. `null` is "not measured" and must be excluded from a
 * ranking rather than sorted to the bottom of one — a model with no coding
 * score is not the worst coder.
 */
export interface ModelBenchmark {
  /** AA's own slug for the entry this model was matched to, and its name
   *  there. Shown so a reader can check the pairing — AA benchmarks each
   *  reasoning effort separately and the sync picks one. */
  aa_slug: string;
  aa_name: string;
  /** Composite indices, 0-100. */
  intelligence: number | null;
  coding: number | null;
  math: number | null;
  /** Individual evaluations, 0-1 fractions. */
  mmlu_pro: number | null;
  gpqa: number | null;
  hle: number | null;
  livecodebench: number | null;
  scicode: number | null;
  aime: number | null;
  /** Median throughput, output tokens per second. */
  output_tps: number | null;
  /** Median time to first token, seconds. */
  ttft_seconds: number | null;
  /** The vendor's list price as AA measured it — **USD per 1M tokens**, not
   *  the platform's micro-USDT per 1k. The two units are deliberately not
   *  reconciled server-side so it stays obvious which side of a
   *  "cheaper than list" comparison came from where. */
  aa_price_input: number | null;
  aa_price_output: number | null;
  aa_price_blended: number | null;
}

/** One row of `GET /api/v1/market/rankings`: a tradable model, its live market
 *  price, and its scores if it has any. */
export interface RankingModel {
  model: string;
  model_id: string;
  provider: string;
  display_name: string;
  context_length: number;
  /** Market price as a fraction of list price, in [0.1, 1.0]. */
  ratio: number;
  /** `1 - ratio`. */
  discount: number;
  /** Micro-USDT per 1k tokens. */
  market_input: number;
  market_output: number;
  ref_input: number;
  ref_output: number;
  supply_capacity_tokens: number;
  online_lanes: number;
  calls_last_minute: number;
  /** Absent for models Artificial Analysis does not score — image, audio,
   *  search and deep-research variants, mostly. */
  bench: ModelBenchmark | null;
}

/** Response of `GET /api/v1/market/rankings`. */
export interface RankingsResp {
  models: RankingModel[];
  /** When the scores were last pulled, unix seconds; 0 if never. */
  benchmarks_updated_ts: number;
  /** Credit the benchmark source requires wherever its data is shown. It
   *  travels with the data so a UI cannot quietly drop it. */
  attribution: { name: string; url: string };
}

/** One point on a model's price chart. */
export interface PricePoint {
  /** Bucket start, unix seconds. */
  ts: number;
  /** Mean ratio over the bucket (equal to the ratio itself for minutes). */
  ratio: number;
  ratio_min: number;
  ratio_max: number;
  /** Mean online subscriptions over the bucket. */
  lanes: number;
  /** Total calls in the bucket. */
  calls: number;
  ref_input: number;
  ref_output: number;
}

/** Chart granularities `GET /api/v1/market/price-history` accepts. */
export type PriceGranularity = "minute" | "hour" | "day" | "month";

/** One country on the world map (`GET /api/v1/market/globe`, `regions`).
 *
 *  Country-level only: the endpoint aggregates `users.region` and never
 *  identifies an account. `"ZZ"` is every user who declared no country. */
export interface GlobeRegion {
  /** ISO 3166-1 alpha-2, or `ZZ` for "not set". */
  region: string;
  users: number;
  providers: number;
  consumers: number;
  tokens: number;
  amount_usdt: number;
  tasks: number;
}

/** Tokens that moved from a seller's country to a buyer's country. */
export interface GlobeFlow {
  from: string;
  to: string;
  tokens: number;
  tasks: number;
  amount_usdt: number;
}

/** Response of `GET /api/v1/market/globe`. */
export interface Globe {
  minutes: number;
  regions: GlobeRegion[];
  flows: GlobeFlow[];
}

/** How far a scan-to-pay session has got (server: `session_status_name`).
 *
 *  `matched` means a transfer was seen on chain; `credited` means it also
 *  cleared confirmations and reached the balance. They are distinct because a
 *  user watching the sheet wants to know the money arrived several minutes
 *  before it is spendable. */
export type DepositSessionStatus = "pending" | "matched" | "credited" | "expired";

/** The transfer a session caught, once one lands. */
export interface DepositSessionTx {
  tx_hash: string;
  /** What actually arrived — an exchange takes its own fee out of the figure
   *  the user typed, so this rarely equals the session's `amount`. */
  amount: number | null;
  confirmations: number | null;
  credited: boolean;
}

/** `POST /api/v1/wallet/deposit-session` and
 *  `GET /api/v1/wallet/deposit-session/:ref`.
 *
 *  A session is a view object: it tracks one top-up so the page can announce
 *  the arrival itself. It holds no funds — the same deposit is credited
 *  identically with no session at all. */
export interface DepositSession {
  ref: string;
  chain: string;
  address: string;
  /** Requested micro-USDT, or null for "any amount". */
  amount: number | null;
  /** Wallet-ready payment request (Solana Pay). Null on rails with no scheme
   *  worth scanning — TRON — where the bare address is the better QR. */
  pay_uri: string | null;
  status: DepositSessionStatus;
  created_ts: number;
  expires_ts: number;
  deposit: DepositSessionTx | null;
}

/** Response of `GET /api/v1/market/price-history`. */
export interface PriceHistory {
  model: string;
  model_id: string;
  display_name: string;
  granularity: string;
  ratio: number;
  discount: number;
  ref_prices: { input: number; output: number; cache_read: number; cache_write: number };
  market_prices: { input: number; output: number; cache_read: number; cache_write: number };
  points: PricePoint[];
}

/** One point of a featured model's sparkline — `ts` and `ratio`, nothing else.
 *
 *  Deliberately not a `PricePoint`: the ticker draws a single line in ~120px,
 *  so the six other fields would be bytes on the wire for pixels nobody renders.
 */
export interface SparkPoint {
  /** Bucket start, unix seconds. */
  ts: number;
  ratio: number;
}

/** One card of `GET /api/v1/market/featured`. */
export interface FeaturedModel {
  model: string;
  model_id: string;
  provider: string;
  display_name: string;
  /** Market price as a fraction of list price, in [0.1, 1.0]. */
  ratio: number;
  /** `1 - ratio`. */
  discount: number;
  /**
   * Change in `ratio` over the last 24h, as a signed fraction (0.05 = +5%).
   *
   * `null` when the model has no history to compare against — a model priced
   * for the first time has no 24h change, and rendering that as `0.00%` would
   * claim a measurement that was never made.
   */
  change_24h: number | null;
  /** Micro-USDT per 1k tokens, at the current ratio. */
  market_prices: { input: number; output: number };
  /** The vendor's list price, same units. */
  ref_prices: { input: number; output: number };
  /** Oldest first, ready to draw. Empty for a model with no history yet. */
  points: SparkPoint[];
}

/** Response of `GET /api/v1/market/featured`. */
export interface FeaturedResp {
  models: FeaturedModel[];
  /** The bucket size of every `points` array — currently always `"hour"`. */
  granularity: PriceGranularity;
}

/** One row of `GET /api/v1/admin/settings`. */
export interface AppSetting {
  key: string;
  value: unknown;
  updated_ts: number;
  updated_by: number | null;
}

// ── API keys ────────────────────────────────────────────────────────

/**
 * One row of `GET /api/v1/apikeys` (`asale-server/src/api/apikeys.rs`).
 *
 * The key itself is never in here. `key_preview` is the masked form the list
 * shows; the plaintext comes back only from `POST /apikeys` (once, at creation)
 * and from `POST /apikeys/:id/reveal`.
 */
export interface ApiKeyRow {
  id: number;
  /** The owner-chosen name. May be empty. */
  label: string;
  /** e.g. `sk-asale-E1T3••••nwo4` — the tail is what tells two keys apart.
   *
   *  Keys minted before the server stored previews have no recoverable
   *  plaintext, so those read `sk-asale-••••••••`: all shape, no tail. They are
   *  exactly the rows with `revealable: false`, which is the signal to explain
   *  the bullets rather than let them look like a rendering bug. */
  key_preview: string;
  /** This row is the key *this machine's* proxy and CLIs are holding. Always
   *  false on the web console, which has no local key to compare against. */
  held?: boolean;
  /** Raw column: 1 active, 2 disabled. Prefer `enabled`. */
  status: number;
  /** The owner-facing on/off switch. */
  enabled: boolean;
  /** The key the desktop app hands to the tools it buys through. */
  is_default: boolean;
  /** RFC 3339, or null for "never expires". */
  expires_at: string | null;
  /** Computed server-side against the server clock, not the browser's. */
  expired: boolean;
  /** `enabled && !expired` — what actually authenticates. */
  usable: boolean;
  created_at: string;
  /** False for keys minted before the sealed copy existed: those cannot be
   *  shown again, only replaced. */
  revealable: boolean;
  /** The highest market price this key buys at, in whole percent of the
   *  vendor's list price. 100 = list price = no ceiling, which is what the
   *  market itself is capped at. Requests above it are refused
   *  (`price_above_cap`) instead of being served at a price nobody agreed to. */
  max_ratio_pct: number;
}

/** `GET /api/v1/apikeys`. */
export interface ApiKeyList {
  keys: ApiKeyRow[];
  /** How many keys one account may hold, so the UI can say so before the 409. */
  max_keys: number;
  /** This account is not actually held to `max_keys` (administrators). Stated
   *  rather than folded into the number, so a console never shows an exempt
   *  account "12 / 10" beside a button it has disabled for no reason. */
  unlimited: boolean;
  /** What "no price ceiling" is spelled as, and the lowest ceiling worth
   *  setting — stated by the server so no frontend hard-codes the range. */
  max_ratio_pct_default: number;
  max_ratio_pct_min: number;
}

/** `POST /api/v1/apikeys` — the one moment the plaintext is returned. */
export interface ApiKeyCreated {
  id: number;
  key: string;
  label: string;
  key_preview: string;
  is_default: boolean;
  expires_at: string | null;
  /** The ceiling this key was minted with, in whole percent of list price. */
  max_ratio_pct: number;
}

/** `PATCH /api/v1/apikeys/:id`. */
export interface ApiKeyUpdated {
  key: ApiKeyRow;
  /** The default moved onto this key. Whatever tools are buying through this
   *  account are now holding the wrong credential — the desktop client offers
   *  to rewrite them, the web console can only say so. */
  default_moved: boolean;
}

// ── errors ──────────────────────────────────────────────────────────

/**
 * The body of every non-2xx REST response (`asale-server/src/error.rs`).
 *
 * `message` is English. It exists so a curl user and an untranslated build see
 * something readable, but it is **not** what a UI should render: `key` names an
 * entry in the frontend's own message catalog and `params` carries the values
 * that entry interpolates, so the user reads the failure in their language. Use
 * the shared helpers (`errorText` in each app) rather than reaching for
 * `message` directly.
 *
 * `key` is absent for messages with no catalog entry — internal invariants and
 * upstream provider text, which fall back to `message`.
 */
export interface ApiErrorBody {
  message: string;
  /** Coarse machine tag: `unauthorized`, `payment_required`, … Branch on this. */
  code: string;
  /** Catalog key, e.g. `errors.wallet.insufficientBalance`. */
  key?: string;
  /** Interpolation values named by the catalog entry. */
  params?: Record<string, string | number | boolean>;
}

export interface ApiErrorEnvelope {
  error: ApiErrorBody;
}
