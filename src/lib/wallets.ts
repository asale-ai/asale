// The wallets a top-up can be paid from, and which QR each of them can read.
//
// This is the whole point of the picker: a QR code is not one thing. A Solana
// Pay URI opens Phantom straight into a filled-in confirmation screen, and is
// unreadable to Binance's scanner. A bare address is readable by everything and
// prefills nothing. Showing one code and hoping is how people end up sending
// the wrong asset, so the list states per wallet which code it gets.
//
// `qr: "pay"` is claimed only for wallets that actually implement Solana Pay.
// Guessing wrong here fails *closed* — the wallet refuses to scan at all, which
// is worse than the bare address it replaced — so an unverified wallet belongs
// in the "address" group. Exchanges never get a payment URI: a withdrawal
// screen takes an address, not a payment request.
//
// Kept in sync with asale-web's `src/lib/wallets.ts` — same ids, same
// ordering, same copy keys. If you change one, change the other. (Runtime code
// is not shared between the two frontends; only types are — see
// `shared/api-types.ts`.)

/** Which QR payload a wallet can read. */
export type QrKind = "pay" | "address";

export interface WalletBrand {
  id: string;
  /** Brand name — shown as-is, never translated. */
  name: string;
  /** Self-custody wallet vs. exchange account. They are funded in completely
   *  different ways, so the sheet groups and instructs them separately. */
  kind: "wallet" | "exchange";
  /** Rails this wallet can send USDT over, by server chain id. */
  chains: string[];
  qr: QrKind;
  /** Brand colour for the tile. */
  color: string;
  /** Letter in the tile. Deliberately not a logo: shipping trademarked marks
   *  is a licensing question, and a coloured initial reads just as fast. */
  mark: string;
}

/** Ordered most-likely-first. Solana Pay wallets lead, because they are the
 *  only ones that can prefill the amount — the flow worth steering people to. */
export const WALLETS: WalletBrand[] = [
  // ── Solana Pay ────────────────────────────────────────────────────────
  { id: "phantom", name: "Phantom", kind: "wallet", chains: ["solana"], qr: "pay", color: "#AB9FF2", mark: "P" },
  { id: "solflare", name: "Solflare", kind: "wallet", chains: ["solana"], qr: "pay", color: "#FC7227", mark: "S" },
  { id: "backpack", name: "Backpack", kind: "wallet", chains: ["solana"], qr: "pay", color: "#E33E3F", mark: "B" },
  // OKX's Web3 wallet reads Solana Pay; the exchange app below does not.
  { id: "okx_wallet", name: "OKX Wallet", kind: "wallet", chains: ["solana"], qr: "pay", color: "#2A2A2E", mark: "O" },

  // ── Self-custody, address-only ────────────────────────────────────────
  { id: "okx_wallet_tron", name: "OKX Wallet", kind: "wallet", chains: ["tron"], qr: "address", color: "#2A2A2E", mark: "O" },
  { id: "tronlink", name: "TronLink", kind: "wallet", chains: ["tron"], qr: "address", color: "#1E7BE3", mark: "T" },
  { id: "trust", name: "Trust Wallet", kind: "wallet", chains: ["solana", "tron"], qr: "address", color: "#3375BB", mark: "T" },
  { id: "imtoken", name: "imToken", kind: "wallet", chains: ["tron"], qr: "address", color: "#11C4D1", mark: "i" },
  { id: "tokenpocket", name: "TokenPocket", kind: "wallet", chains: ["tron"], qr: "address", color: "#2980FE", mark: "T" },

  // ── Exchange accounts ─────────────────────────────────────────────────
  { id: "binance", name: "Binance", kind: "exchange", chains: ["solana", "tron"], qr: "address", color: "#F0B90B", mark: "B" },
  { id: "okx", name: "OKX", kind: "exchange", chains: ["solana", "tron"], qr: "address", color: "#2A2A2E", mark: "O" },
  { id: "bybit", name: "Bybit", kind: "exchange", chains: ["solana", "tron"], qr: "address", color: "#F7A600", mark: "B" },
  { id: "gate", name: "Gate.io", kind: "exchange", chains: ["solana", "tron"], qr: "address", color: "#2354E6", mark: "G" },
  { id: "bitget", name: "Bitget", kind: "exchange", chains: ["solana", "tron"], qr: "address", color: "#1DA2B4", mark: "B" },
  // Coinbase has no TRON support at all, so it is Solana-only here.
  { id: "coinbase", name: "Coinbase", kind: "exchange", chains: ["solana"], qr: "address", color: "#0052FF", mark: "C" },

  // The escape hatch, and the reason it is last: an address QR works
  // everywhere, so nobody is stuck if their wallet is not listed.
  { id: "other", name: "", kind: "wallet", chains: ["solana", "tron"], qr: "address", color: "#6B7280", mark: "+" },
];

/** The wallets that can fund `chain`, in catalogue order. */
export function walletsFor(chain: string): WalletBrand[] {
  return WALLETS.filter((w) => w.chains.includes(chain));
}

/** What to put in the QR for this wallet.
 *
 *  Falls back to the address whenever a payment URI is unavailable — the rail
 *  has no scheme, or the session has not been created yet. A QR that encodes
 *  the address is always correct; it just asks more of the user. */
export function qrPayload(wallet: WalletBrand | null, payUri: string | null, address: string): string {
  return wallet?.qr === "pay" && payUri ? payUri : address;
}
