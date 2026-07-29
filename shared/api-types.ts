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
