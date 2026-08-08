// Country list for the sign-up and profile pickers.
//
// The country a user declares is the *only* source of location the platform
// stores: asale-server runs no geolocation, never reads a client IP, and the
// landing page's world map aggregates `users.region` and nothing else. So it
// has to be asked for, and asked for in a way that yields one canonical value —
// hence a picker over ISO 3166-1 alpha-2 codes rather than the free-text field
// this replaces, which happily stored "us", "USA" and "United States" as three
// different countries.
//
// The picker's *starting* value is guessed, purely to save a scroll through 250
// rows: `detectCountryByIp` asks a third-party lookup service what country this
// browser's address is in, and `guessCountry` reads the browser's own language
// when that fails or is refused. Both are pre-selections only — nothing is sent
// anywhere until the user submits, and what gets stored is whatever they leave
// in the field.
//
// Only the codes live here. Names come from `Intl.DisplayNames`, so all four
// locales get correctly translated country names (and the browser's own
// spelling) without four × 251 lines of translation to maintain. The server
// validates the same shape in `api::normalize_region`.
//
// KEEP IN SYNC with asale-web/src/lib/countries.ts, which is the same file.
// The rest of this directory is shared with the web app by import, but that
// works only because those modules are types and vanish before bundling. This
// one is runtime code and each bundler insists on finding it inside its own
// project root, so the table is duplicated rather than reached across.

/**
 * Current ISO 3166-1 alpha-2 codes, as resolved by ICU. Deprecated entries
 * (`AN`, `SU`, `YU`, …), groupings (`EU`, `UN`, `QO`), exceptional
 * reservations (`AC`, `EA`, `IC`, …) and `ZZ` are excluded: `ZZ` is what an
 * unset region already aggregates into, so offering it as a choice would just
 * be a second way to say nothing.
 */
export const COUNTRY_CODES: readonly string[] = [
  "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT",
  "AU", "AW", "AX", "AZ", "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI",
  "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS", "BT", "BV", "BW", "BY",
  "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
  "CO", "CQ", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK",
  "DM", "DO", "DZ", "EC", "EE", "EG", "EH", "ER", "ES", "ET", "FI", "FJ",
  "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF", "GG", "GH", "GI",
  "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK",
  "HM", "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ",
  "IR", "IS", "IT", "JE", "JM", "JO", "JP", "KE", "KG", "KH", "KI", "KM",
  "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC", "LI", "LK", "LR",
  "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH",
  "MK", "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV",
  "MW", "MX", "MY", "MZ", "NA", "NC", "NE", "NF", "NG", "NI", "NL", "NO",
  "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG", "PH", "PK", "PL",
  "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU",
  "RW", "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL",
  "SM", "SN", "SO", "SR", "SS", "ST", "SV", "SX", "SY", "SZ", "TC", "TD",
  "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO", "TR", "TT", "TV",
  "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG",
  "VI", "VN", "VU", "WF", "WS", "XK", "YE", "YT", "ZA", "ZM", "ZW",
];

export interface CountryOption {
  /** ISO 3166-1 alpha-2, the value the server stores. */
  code: string;
  /** Country name in the reader's language. */
  name: string;
  /** Flag as a regional-indicator pair — see `flagEmoji`. */
  flag: string;
  /**
   * Lowercased haystack the search box matches against: the localized name, the
   * English name and the code. The English name is in there so a reader on the
   * Chinese UI can still type "japan", and a reader anywhere can type "JP".
   */
  search: string;
}

/** Whether a string is shaped like a country code the server would accept. */
export function isCountryCode(value: string): boolean {
  return /^[A-Za-z]{2}$/.test(value.trim());
}

/**
 * The flag for a country code, as the two regional-indicator symbols the code's
 * letters map to. No image assets and no lookup table: every platform that has
 * a flag font renders the pair as one flag, and every platform that does not
 * (Windows ships no colour flags) falls back to the two boxed letters — which
 * is the country code, i.e. exactly what the row shows next to it anyway.
 */
export function flagEmoji(code: string): string {
  if (!/^[A-Za-z]{2}$/.test(code)) return "";
  return String.fromCodePoint(
    ...[...code.toUpperCase()].map((ch) => 0x1f1e6 + ch.charCodeAt(0) - 65)
  );
}

/**
 * Localized country names, sorted the way the reader's language sorts them.
 * Falls back to the bare code on a runtime without `Intl.DisplayNames`.
 */
export function countryOptions(locale: string): CountryOption[] {
  const display = regionNames(locale);
  // Built once for the whole list rather than per row: `Intl.DisplayNames` is
  // not free, and 250 rows × 2 locales × every keystroke adds up.
  const english = locale.toLowerCase().startsWith("en") ? display : regionNames("en");
  const collator = new Intl.Collator(locale);
  return COUNTRY_CODES.map((code) => {
    const name = display?.of(code) ?? code;
    const en = english?.of(code) ?? code;
    return { code, name, flag: flagEmoji(code), search: `${name} ${en} ${code}`.toLowerCase() };
  }).sort((a, b) => collator.compare(a.name, b.name));
}

function regionNames(locale: string): Intl.DisplayNames | null {
  try {
    return new Intl.DisplayNames([locale], { type: "region", fallback: "code" });
  } catch {
    return null;
  }
}

/**
 * Whether `option` matches everything the user has typed. Terms are matched
 * independently so word order and spacing do not matter ("korea south" finds
 * South Korea), and each term only has to appear somewhere in the haystack, so
 * a partial name works before it is finished.
 */
export function countryMatches(option: CountryOption, query: string): boolean {
  const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return true;
  return terms.every((term) => option.search.includes(term));
}

/**
 * A starting guess for the picker, taken from the browser's own language
 * settings (`zh-TW` → `TW`). Offline and instant, which is why it is the value
 * the field is *born* with; `detectCountryByIp` may replace it a moment later.
 */
export function guessCountry(): string {
  if (typeof navigator === "undefined") return "";
  const tags = [...(navigator.languages ?? []), navigator.language].filter(Boolean);
  for (const tag of tags) {
    try {
      const region = new Intl.Locale(tag).maximize().region;
      if (region && COUNTRY_CODES.includes(region)) return region;
    } catch {
      // A malformed tag is not worth failing over; try the next one.
    }
  }
  return "";
}

/**
 * Free, key-less, CORS-open country lookups, tried in order. Each is a plain
 * GET that answers with the country of the address the request came from; the
 * first one to answer with a code we recognise wins.
 *
 * More than one because these are somebody else's free tier: they run out of
 * quota, they go down, and an ad blocker will happily eat the best-known of
 * them. A picker that pre-selects nothing is a fine outcome, so every failure
 * here is silent.
 */
const IP_LOOKUPS: readonly { url: string; field: string }[] = [
  { url: "https://ipwho.is/?fields=country_code", field: "country_code" },
  { url: "https://get.geojs.io/v1/ip/country.json", field: "country" },
  { url: "https://api.country.is/", field: "country" },
];

/** Where the answer is parked so a second picker in the same session is free. */
const IP_CACHE_KEY = "asale.ip-country";

/**
 * The country this browser's IP address is in, or "" if nothing answered.
 *
 * Note what leaves the machine: the request itself, whose source address is the
 * only payload. Nothing about the user or the account is sent, the answer never
 * reaches asale-server, and it is a pre-selection the user can change. Only
 * call this when the field would otherwise start empty.
 */
export async function detectCountryByIp(timeoutMs = 2500): Promise<string> {
  if (typeof fetch === "undefined") return "";

  const cached = readCache();
  if (cached !== null) return cached;

  for (const { url, field } of IP_LOOKUPS) {
    const code = await lookupCountry(url, field, timeoutMs);
    if (code) {
      writeCache(code);
      return code;
    }
  }
  // Cache the failure too: three timeouts is ~7s, and paying it again on the
  // next page in the same session buys nothing.
  writeCache("");
  return "";
}

async function lookupCountry(url: string, field: string, timeoutMs: number): Promise<string> {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), timeoutMs);
  try {
    const res = await fetch(url, { signal: abort.signal, referrerPolicy: "no-referrer" });
    if (!res.ok) return "";
    const body = (await res.json()) as Record<string, unknown>;
    const code = String(body[field] ?? "").toUpperCase();
    // Guard the code rather than trust it: `ZZ`, a lower-cased value or a
    // retired code would all be stored happily and then reject at the server.
    return COUNTRY_CODES.includes(code) ? code : "";
  } catch {
    return "";
  } finally {
    clearTimeout(timer);
  }
}

function readCache(): string | null {
  try {
    return sessionStorage.getItem(IP_CACHE_KEY);
  } catch {
    return null; // Private mode, or storage disabled. Just do the lookup.
  }
}

function writeCache(code: string) {
  try {
    sessionStorage.setItem(IP_CACHE_KEY, code);
  } catch {
    // Not being able to cache is not a reason to fail the lookup.
  }
}
