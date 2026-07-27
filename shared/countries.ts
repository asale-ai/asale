// Country list for the sign-up and profile pickers.
//
// The country a user declares is the *only* source of location in the whole
// system: asale-server runs no geolocation, never reads a client IP, and the
// landing page's world map aggregates `users.region` and nothing else. So it
// has to be asked for, and asked for in a way that yields one canonical value —
// hence a picker over ISO 3166-1 alpha-2 codes rather than the free-text field
// this replaces, which happily stored "us", "USA" and "United States" as three
// different countries.
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
  code: string;
  name: string;
}

/** Whether a string is shaped like a country code the server would accept. */
export function isCountryCode(value: string): boolean {
  return /^[A-Za-z]{2}$/.test(value.trim());
}

/**
 * Localized country names, sorted the way the reader's language sorts them.
 * Falls back to the bare code on a runtime without `Intl.DisplayNames`.
 */
export function countryOptions(locale: string): CountryOption[] {
  let display: Intl.DisplayNames | null = null;
  try {
    display = new Intl.DisplayNames([locale], { type: "region", fallback: "code" });
  } catch {
    display = null;
  }
  const collator = new Intl.Collator(locale);
  return COUNTRY_CODES.map((code) => ({ code, name: display?.of(code) ?? code })).sort((a, b) =>
    collator.compare(a.name, b.name)
  );
}

/**
 * A starting guess for the picker, taken from the browser's own language
 * settings (`zh-TW` → `TW`) — never from the IP address, which is the whole
 * point of the promise on the landing page. It is only a pre-selection: the
 * value that gets saved is whatever the user leaves in the field.
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
