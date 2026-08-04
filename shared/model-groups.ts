// Model id → family + version, and the grouping every model list should use.
//
// The catalog is ~350 rows wide and most of that width is history: seven
// `claude-opus-*`, seven `gpt-*`, six `glm-*`, each of them also duplicated as
// a `:batch` variant. Listing all of it flat makes the newest model — the one
// a buyer almost always wants — just another line in a wall of near-identical
// names. So lists collapse a family to its newest member and keep the older
// ones one click away.
//
// Doing that needs an answer to "same model, older version?", and the ids are
// the only thing that carries it. There is no version field: the platform
// stores what the upstream catalog publishes, and upstreams write versions
// into the id in at least four shapes —
//
//   anthropic/claude-opus-4-8        dashes
//   openai/gpt-5.4-mini              dots, version in the middle
//   deepseek/deepseek-chat-v3.1      a `v` prefix
//   openai/gpt-4o-2024-11-20         a dated snapshot of an alias
//
// — so the parse below splits an id into the words that name the model and the
// numbers that version it, rather than pattern-matching any one vendor.
//
// Runtime code, unlike the rest of `shared/`: `asale-client` imports it as
// `@shared/model-groups` (aliased in vite.config.ts). asale-web resolves
// `@shared/*` for types only, so if the web console ever needs this it takes
// the same route `src/lib/countries.ts` did — a copy kept in sync.

/** A model id split into the parts that decide what it is and how new it is. */
export interface ModelIdParts {
  /** Vendor half of a `vendor/model` id, `""` when the id is unqualified. */
  vendor: string;
  /** The naming words with every version number removed, e.g. `claude-opus`. */
  family: string;
  /** Suffix after `:` — a packaging variant like `batch` or `free`, not a version. */
  variant: string;
  /** Version segments, most significant first: `4-8` and `4.8` both give `[4, 8]`. */
  version: number[];
  /** Trailing date/build stamp (`2024-11-20`, `0905`), `""` when the id has none. */
  snapshot: string;
  /** True for a moving alias like `claude-opus-latest` — always the newest of its family. */
  alias: boolean;
  /** Stable group key. Ids sharing it are versions of one model. */
  key: string;
}

/**
 * A trailing release stamp rather than a version: a full date, a `YYMM`/`MMDD`
 * build number, or a `MM-DD` pair. Only ever at the end, and only in groups of
 * ≥ 4 digits (or two 2-digit halves), so `claude-opus-4-1` and
 * `mistral-medium-3-5` keep their versions.
 */
const SNAPSHOT_RE = /-(\d{4}-\d{2}-\d{2}|\d{8}|\d{6}|\d{4}|\d{2}-\d{2})$/;

/** `v3` / `V12` — a version segment wearing a prefix. */
const V_PREFIXED_RE = /^v(\d+)$/i;

/** Names an always-current pointer at a family rather than a release of it. */
const ALIAS_TOKEN = "latest";

/**
 * Split a model id. Never throws and never returns an empty `key`: an id that
 * parses to nothing recognisable is its own family of one, which is the safe
 * outcome — it stays visible in every list.
 */
export function parseModelId(id: string): ModelIdParts {
  const raw = (id ?? "").trim().toLowerCase();

  const slash = raw.lastIndexOf("/");
  const vendor = slash >= 0 ? raw.slice(0, slash) : "";
  let rest = slash >= 0 ? raw.slice(slash + 1) : raw;

  const colon = rest.indexOf(":");
  const variant = colon >= 0 ? rest.slice(colon + 1) : "";
  if (colon >= 0) rest = rest.slice(0, colon);

  const stamp = rest.match(SNAPSHOT_RE);
  const snapshot = stamp ? stamp[1] : "";
  if (stamp) rest = rest.slice(0, -stamp[0].length);

  const words: string[] = [];
  const version: number[] = [];
  let alias = false;
  for (const token of rest.split(/[-.]/)) {
    if (!token) continue;
    if (token === ALIAS_TOKEN) { alias = true; continue; }
    if (/^\d+$/.test(token)) { version.push(Number(token)); continue; }
    const v = token.match(V_PREFIXED_RE);
    if (v) { version.push(Number(v[1])); continue; }
    words.push(token);
  }

  // An id made only of numbers (or only of `latest`) has no words to name it —
  // fall back to the id itself so it cannot collide with an unrelated model.
  const family = words.join("-") || rest || raw;
  return {
    vendor,
    family,
    variant,
    version,
    snapshot,
    alias,
    key: `${vendor}/${family}${variant ? `:${variant}` : ""}`,
  };
}

/** Digits only, for comparing two stamps of the same shape. */
const stampValue = (s: string) => s.replace(/\D/g, "");

/**
 * Order two parsed ids newest-first, for use as an `Array#sort` comparator.
 *
 * Version numbers decide it, segment by segment, with a missing segment read as
 * `0` so `4` sorts below `4.6`. Segments compare as integers, not text, so
 * `4.20` is newer than `4.5` — every vendor here versions in segments, not
 * decimals.
 *
 * Below that, an undated id beats a dated one: `gpt-4o` is the pointer the
 * vendor moves forward and `gpt-4o-2024-05-13` is a frozen snapshot of it, so
 * the pointer is what a list should lead with. (A family whose newest release
 * only exists dated — `mistral-large-2512` — therefore shows its plain alias
 * first; the dated one is one click away, which is the same cost either way.)
 */
export function compareModelRecency(a: ModelIdParts, b: ModelIdParts): number {
  if (a.alias !== b.alias) return a.alias ? -1 : 1;
  const depth = Math.max(a.version.length, b.version.length);
  for (let i = 0; i < depth; i++) {
    const d = (b.version[i] ?? 0) - (a.version[i] ?? 0);
    if (d !== 0) return d;
  }
  if (!a.snapshot !== !b.snapshot) return a.snapshot ? 1 : -1;
  if (a.snapshot !== b.snapshot) {
    const [x, y] = [stampValue(a.snapshot), stampValue(b.snapshot)];
    // Different stamp shapes (`0905` vs `20260420`) are not comparable as
    // numbers; the longer, more precise one is the later convention.
    if (x.length !== y.length) return y.length - x.length;
    return y.localeCompare(x);
  }
  return 0;
}

/** One model family: its newest member, plus the versions it supersedes. */
export interface ModelFamily<T> {
  /** `ModelIdParts.key` shared by every member — stable across refreshes. */
  key: string;
  /** The newest member, by `compareModelRecency`. */
  latest: T;
  /** The rest, newest first. Empty for a family with a single version. */
  older: T[];
  /** `[latest, ...older]`. */
  all: T[];
}

/**
 * Group items by model family, newest first within each.
 *
 * Group order follows the position of each family's *newest* member in `items`,
 * so a caller that sorted its list (by name, by price) keeps that order for the
 * rows it will actually show.
 *
 * Grouping deliberately runs *after* filtering, not before: pass the rows the
 * user can currently see. A search for `opus 4.1` then leaves 4.1 as the newest
 * of what matched and it shows up, instead of being hidden under a version the
 * search excluded.
 *
 * `idOf` should return a vendor-qualified id (`anthropic/claude-opus-4-6`)
 * whenever the vendor is known, so two vendors' same-named models stay apart.
 */
export function groupModelsByFamily<T>(items: readonly T[], idOf: (item: T) => string): ModelFamily<T>[] {
  const buckets = new Map<string, { parts: ModelIdParts; item: T; index: number }[]>();
  items.forEach((item, index) => {
    const parts = parseModelId(idOf(item));
    const bucket = buckets.get(parts.key);
    if (bucket) bucket.push({ parts, item, index });
    else buckets.set(parts.key, [{ parts, item, index }]);
  });

  const groups = [...buckets.entries()].map(([key, members]) => {
    // Ties (an id repeated, or two ids parsing the same) fall back to input
    // order, so the list never reshuffles between renders.
    const sorted = [...members].sort(
      (a, b) => compareModelRecency(a.parts, b.parts) || a.index - b.index,
    );
    const all = sorted.map((m) => m.item);
    return { key, latest: all[0], older: all.slice(1), all, at: sorted[0].index };
  });

  groups.sort((a, b) => a.at - b.at);
  return groups.map(({ key, latest, older, all }) => ({ key, latest, older, all }));
}
