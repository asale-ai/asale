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
  supply_capacity_tokens: number;
  demand: number;
}
